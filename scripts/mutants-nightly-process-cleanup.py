#!/usr/bin/env python3
"""Report and stop orphaned test services on a dedicated Linux runner."""

from __future__ import annotations

import argparse
import os
import signal
import time
from pathlib import Path


PROCESS_NAMES = {"tmux", "tmux: server", "prismattyc", "pmux", "pmuxd", "Xvfb"}


def candidates() -> list[tuple[int, str, str]]:
    """Return owned test-service processes from procfs."""
    uid = os.getuid()
    found: list[tuple[int, str, str]] = []
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        pid = int(entry.name)
        if pid == os.getpid():
            continue
        try:
            if entry.stat().st_uid != uid:
                continue
            name = (entry / "comm").read_text(encoding="utf-8").strip()
            if name not in PROCESS_NAMES and not name.startswith(("prismattyc", "pmux")):
                continue
            args = (entry / "cmdline").read_bytes().replace(b"\0", b" ").decode(
                "utf-8", errors="replace"
            ).strip()
        except (FileNotFoundError, PermissionError, ProcessLookupError):
            continue
        found.append((pid, name, args))
    return found


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--phase", choices=("before", "after"), required=True)
    args = parser.parse_args()

    found = candidates()
    print(f"orphan cleanup ({args.phase}): {len(found)} test service process(es)")
    for pid, name, command in found:
        print(f"  pid={pid} name={name} command={command}")
    if args.phase == "before":
        return 0

    for pid, _, _ in found:
        try:
            os.kill(pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
    deadline = time.monotonic() + 3
    while time.monotonic() < deadline and candidates():
        time.sleep(0.1)
    for pid, _, _ in candidates():
        try:
            os.kill(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
