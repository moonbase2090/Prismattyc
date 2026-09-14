#!/usr/bin/env python3
"""Measure PTY output through a terminal's cursor-position response barrier."""
import fcntl
import json
import os
from pathlib import Path
import select
import struct
import termios
import time
import tty


def emit(data):
    view = memoryview(data)
    while view:
        view = view[os.write(1, view):]


def query():
    emit(b"\x1b[6n")
    response = b""
    deadline = time.monotonic() + 20
    while not response.endswith(b"R"):
        if time.monotonic() >= deadline:
            raise TimeoutError("cursor-position response")
        if select.select([0], [], [], 0.1)[0]:
            part = os.read(0, 1024)
            if not part:
                raise EOFError("terminal input closed")
            response += part
    return response.decode("ascii", "replace")


def main():
    output = Path(os.environ["BENCH_RESULT"])
    scale = int(os.environ.get("BENCH_SCALE", "1"))
    if scale < 1:
        raise ValueError("BENCH_SCALE must be positive")

    def record(**row):
        with output.open("a") as stream:
            stream.write(json.dumps(row) + "\n")

    previous = termios.tcgetattr(0)
    tty.setraw(0)
    try:
        query()
        rows, columns, _, _ = struct.unpack(
            "HHHH", fcntl.ioctl(0, termios.TIOCGWINSZ, bytes(8))
        )
        record(
            phase="ready", rows=rows, cols=columns,
            startup_ms=(time.monotonic_ns() - int(os.environ["BENCH_STARTED"])) / 1e6,
        )
        time.sleep(2)
        workloads = [
            ("ascii", b"abcdefghijklmnopqrstuvxyz0123456789 abcdefghijklmnopqrstuvwxyz\r\n" * (50_000 * scale)),
            ("unicode", ("café Ελληνικά 日本語 😀 e\u0301\r\n" * (15_000 * scale)).encode()),
            ("alt-screen", (b"\x1b[H" + b"abcdefghijklmnopqrstuvxyz0123456789\r\n" * 20) * (1_200 * scale)),
        ]
        for label, data in workloads:
            if label == "alt-screen":
                emit(b"\x1b[?1049h")
            started = time.monotonic()
            emit(data)
            cursor = query()
            elapsed = time.monotonic() - started
            record(phase=label, bytes=len(data), seconds=elapsed,
                   mib_per_second=len(data) / elapsed / 1024**2, cursor=cursor)
            if label == "alt-screen":
                emit(b"\x1b[?1049l")
            time.sleep(2)
        timings = []
        for _ in range(60):
            started = time.monotonic_ns()
            query()
            timings.append((time.monotonic_ns() - started) / 1e6)
            time.sleep(0.01)
        timings.sort()
        record(phase="idle-response", p50_ms=timings[30],
               p95_ms=timings[57], max_ms=max(timings))
        time.sleep(3)
        record(phase="done")
    finally:
        termios.tcsetattr(0, termios.TCSANOW, previous)


if __name__ == "__main__":
    main()
