#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Slow-reading pane child for paste-e2e.py.

Raw mode, so pasted bytes arrive unchanged. Prints a tick every 10 ms, which
keeps the host rendering frames, and reads input at about 64 KiB per 20 ms.
Writes everything it received to $PASTE_E2E_RESULT once input has been quiet
for 3 s, or after 30 s with nothing received.
"""
import os
import select
import time
import tty

tty.setraw(0)
os.write(1, b"\x1b[2J")
open(os.environ["PASTE_E2E_READY"], "w").close()
received = bytearray()
last = time.monotonic()
tick = 0
while True:
    ready, _, _ = select.select([0], [], [], 0.01)
    if ready:
        received += os.read(0, 65536)
        last = time.monotonic()
        time.sleep(0.02)
    tick += 1
    os.write(1, f"\rtick {tick} received {len(received)}   ".encode())
    quiet = time.monotonic() - last
    if (received and quiet > 3) or (not received and quiet > 30):
        break
with open(os.environ["PASTE_E2E_RESULT"], "wb") as out:
    out.write(received)
