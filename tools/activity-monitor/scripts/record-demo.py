#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Record the real monitor in a pseudo-terminal as an asciicast v2 file.

The monitor runs unmodified in a pty. This script only writes keys, changes the pty size the
way a pane split does, and records exactly the bytes the monitor writes. Render the cast with
agg; that shows the cell path (pixel lanes need a terminal with graphics, see the README).

A plan is a JSON list of steps, applied in order:
  {"at": 0.0, "size": [140, 34]}          resize the pty (the first step sets the start size)
  {"at": 3.0, "keys": "jj"}               write keys; "\\r" Enter, "\\t" Tab, "\\u001b" Esc
  {"at": 6.0, "signal": [PID, "USR1"]}    signal another process (for example, the demo feed)
  {"at": 9.0, "end": true}                quit with q and stop recording

  record-demo.py --plan plan.json --out demo.cast -- MONITOR ARGS...
"""

import argparse
import codecs
import fcntl
import json
import os
import pty
import select
import signal
import struct
import sys
import termios
import time
from pathlib import Path

# Replies a terminal gives to the monitor's capability probe: a dark background, then primary
# device attributes, which ends the probe. --kitty adds the replies of a Kitty-graphics
# terminal (graphics query accepted, 10x20 pixel cells), so the pixel path runs.
PROBE_REPLIES = [
    (b'\x1b]11;?', b'\x1b]11;rgb:1818/1b1b/2121\x1b\\'),
    (b'\x1b[c', b'\x1b[?62;22c'),
]
KITTY_REPLIES = [
    (b'\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\', b'\x1b_Gi=31;OK\x1b\\'),
    (b'\x1b[16t', b'\x1b[6;20;10t'),
]


def set_size(fd, cols, rows):
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack('HHHH', rows, cols, 0, 0))


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--plan', required=True, type=Path)
    parser.add_argument('--out', required=True, type=Path)
    parser.add_argument('--kitty', action='store_true', help='answer the probe as a Kitty-graphics terminal')
    parser.add_argument('command', nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ['--'] else args.command
    plan = json.loads(args.plan.read_text())
    cols, rows = plan[0]['size']
    pid, fd = pty.fork()
    if pid == 0:
        os.environ.update(TERM='xterm-256color', COLORTERM='truecolor')
        os.environ.pop('TERM_PROGRAM', None)
        os.execvp(command[0], command)
    set_size(fd, cols, rows)
    os.kill(pid, signal.SIGWINCH)
    events = []
    began = time.monotonic()
    pending = list(plan[1:])
    probe_buffer = b''
    # Kitty replies must precede device attributes, which ends the probe.
    # Glyphs can be split across reads; decode incrementally so none are lost.
    decoder = codecs.getincrementaldecoder('utf-8')('replace')
    replies = (KITTY_REPLIES if args.kitty else []) + PROBE_REPLIES
    answered = set()
    finished = False
    with args.out.open('w') as cast:
        cast.write(json.dumps({'version': 2, 'width': cols, 'height': rows,
                               'env': {'TERM': 'xterm-256color'}}) + '\n')
        while True:
            now = time.monotonic() - began
            while pending and pending[0]['at'] <= now:
                step = pending.pop(0)
                if 'size' in step:
                    cols, rows = step['size']
                    set_size(fd, cols, rows)
                    os.kill(pid, signal.SIGWINCH)
                    cast.write(json.dumps([round(now, 6), 'r', f'{cols}x{rows}']) + '\n')
                if 'keys' in step:
                    os.write(fd, step['keys'].encode())
                if 'signal' in step:
                    target, name = step['signal']
                    os.kill(int(target), getattr(signal, f'SIG{name}'))
                if step.get('end'):
                    os.write(fd, b'q')
                    finished = True
            ready, _, _ = select.select([fd], [], [], 0.01)
            if ready:
                try:
                    data = os.read(fd, 65536)
                except OSError:
                    break
                if not data:
                    break
                probe_buffer = (probe_buffer + data)[-512:]
                for query, reply in replies:
                    if query in probe_buffer and query not in answered:
                        os.write(fd, reply)
                        answered.add(query)
                text = decoder.decode(data)
                if text:
                    cast.write(json.dumps([round(time.monotonic() - began, 6), 'o', text]) + '\n')
            if finished and time.monotonic() - began > now + 2:
                break
            done, status = os.waitpid(pid, os.WNOHANG)
            if done:
                break
    try:
        os.kill(pid, signal.SIGTERM)
        os.waitpid(pid, 0)
    except (ProcessLookupError, ChildProcessError):
        pass
    return 0


if __name__ == '__main__':
    sys.exit(main())
