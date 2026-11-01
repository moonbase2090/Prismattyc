#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Start private activity daemons and publish scripted metadata events to them.

Each host label gets its own pmuxd with --experimental-activity and its own socket, in an
isolated scratch tree. Events carry only the schema's metadata: seat, hashed run and event
identifiers, source and kind. The script writes {label: socket} to --sockets, publishes until
--seconds elapse (or forever), and removes its daemons and scratch tree on exit.

  demo-feed.py --bins target/debug --sockets /tmp/sockets.json --host mac --host windows
  demo-feed.py ... --outage windows:200:20   # stop windows's daemon at 200 s for 20 s

SIGUSR1 stops the last host's daemon and SIGUSR2 starts it again, for recording an outage.
"""

import argparse
import hashlib
import json
import os
import random
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

# Seats per host, each with a behaviour profile. Names are neutral role identities.
SEATS = {
    'mac': [('builder', 'busy'), ('reviewer', 'turns'), ('docs', 'turns'),
            ('release', 'flaky'), ('triage', 'idle'), ('design', 'busy')],
    'windows': [('win-build', 'busy'), ('win-proof', 'flaky'), ('win-pack', 'stopper')],
}
TOOL_KINDS = [('command', 0.5), ('edit', 0.3), ('tool', 0.2)]


def opaque(*parts):
    return hashlib.sha256(':'.join(parts).encode()).hexdigest()


class Daemon:
    def __init__(self, bins, scratch, label, env, socket_path=None):
        self.bins, self.label, self.env = bins, label, env
        self.root = scratch / label
        self.root.mkdir(mode=0o700)
        # An attached daemon belongs to someone else: publish only, never start or stop it.
        self.attached = socket_path is not None
        self.socket = Path(socket_path) if self.attached else self.root / 'feed.sock'
        self.process = None

    def start(self):
        if self.attached:
            deadline = time.monotonic() + 15
            while time.monotonic() < deadline:
                try:
                    self.request('ping')
                    return
                except OSError:
                    time.sleep(0.1)
            raise SystemExit(f'{self.label} daemon at --attach did not answer')
        log = (self.root / 'daemon.log').open('ab')
        self.process = subprocess.Popen(
            [str(self.bins / 'pmuxd'), '--socket', str(self.socket), '--cols', '80', '--rows', '24',
             '--experimental-activity', '--', '/bin/sh'],
            cwd=self.root, env=self.env, stdout=log, stderr=log)
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                raise SystemExit(f'{self.label} daemon exited; see {self.root}/daemon.log')
            try:
                self.request('ping')
                return
            except OSError:
                time.sleep(0.05)
        raise SystemExit(f'{self.label} daemon did not answer')

    def stop(self):
        if self.attached:
            return
        if self.process and self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()
        self.process = None

    def alive(self):
        return self.attached or (self.process is not None and self.process.poll() is None)

    def request(self, kind, **fields):
        wire = {'type': kind, 'version': 1, 'request_id': 1, **fields}
        with socket.socket(socket.AF_UNIX) as stream:
            stream.settimeout(3)
            stream.connect(str(self.socket))
            stream.sendall(json.dumps(wire).encode() + b'\n')
            return json.loads(stream.makefile('rb').readline(1 << 20))

    def publish(self, seat, run, kind, source='hooks'):
        event = {'schema_version': 1, 'seat': seat, 'run_id': run,
                 'event_id': opaque(seat, run, kind, str(time.monotonic_ns())),
                 'source': source, 'kind': kind}
        reply = self.request('publish_activity', event=event)
        if reply.get('status') != 'ok':
            raise SystemExit(f'publish rejected: {reply}')


class Seat:
    """A small state machine that emits plausible lifecycle events."""

    def __init__(self, host, name, profile, rng):
        self.host, self.name, self.profile, self.rng = host, name, profile, rng
        self.run = None
        self.state = 'idle'
        self.next_at = time.monotonic() + rng.uniform(0.2, 3)
        self.turn_ends = 0
        self.open_tool = None

    def new_run(self):
        self.run = opaque(self.host.label, self.name, str(self.rng.random()))[:64]

    def step(self, now):
        if now < self.next_at:
            return
        r, p = self.rng, self.profile
        pub = lambda kind, source='hooks': self.host.publish(self.name, self.run, kind, source)
        if self.run is None:
            self.new_run()
            pub('session_started')
            self.state = 'waiting'
            self.next_at = now + r.uniform(1, 4)
            return
        if self.state in ('waiting', 'failed'):
            if p == 'idle' and r.random() < 0.7:
                self.next_at = now + r.uniform(20, 60)
                return
            if p == 'stopper' and r.random() < 0.25:
                pub('session_finished')
                self.run = None
                self.state = 'stopped'
                self.next_at = now + r.uniform(25, 50)
                return
            pub('turn_started')
            self.state = 'working'
            self.turn_ends = now + {'busy': r.uniform(25, 70), 'turns': r.uniform(8, 25),
                                    'flaky': r.uniform(10, 30), 'idle': r.uniform(5, 10),
                                    'stopper': r.uniform(15, 30)}[p]
            self.next_at = now + r.uniform(0.3, 1.5)
            return
        if self.state == 'stopped':
            self.state = 'waiting'
            return
        # working
        if self.open_tool:
            kind = self.open_tool
            self.open_tool = None
            fail = {'flaky': 0.28, 'busy': 0.04}.get(p, 0.02)
            if r.random() < fail:
                pub(f'{kind}_failed')
                self.state = 'failed'
                self.next_at = now + r.uniform(6, 14)
                return
            pub(f'{kind}_finished')
            self.next_at = now + r.uniform(0.2, 2.5)
            return
        if now >= self.turn_ends:
            if p in ('flaky', 'busy') and r.random() < 0.35:
                pub('test_started')
                time.sleep(0.01)
                pub('test_passed' if r.random() < 0.6 else 'test_failed')
            if r.random() < 0.3:
                pub('mail_sent')
            pub('turn_finished', 'notify' if r.random() < 0.5 else 'hooks')
            self.state = 'waiting'
            self.next_at = now + r.uniform(6, 30)
            return
        kind = r.choices([k for k, _ in TOOL_KINDS], [w for _, w in TOOL_KINDS])[0]
        pub(f'{kind}_started')
        self.open_tool = kind
        self.next_at = now + r.uniform(0.4, 6)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--bins', required=True, type=Path)
    parser.add_argument('--sockets', required=True, type=Path)
    parser.add_argument('--host', action='append', default=[], choices=sorted(SEATS))
    parser.add_argument('--attach', action='append', default=[], metavar='LABEL=SOCKET',
                        help="publish that host's seats into an existing daemon started with "
                             '--experimental-activity, without starting or stopping it')
    parser.add_argument('--seconds', type=float, default=0, help='0 runs until interrupted')
    parser.add_argument('--seed', type=int, default=2090)
    parser.add_argument('--outage', action='append', default=[],
                        help='LABEL:AT:FOR stops that daemon AT seconds in, for FOR seconds')
    args = parser.parse_args()
    hosts = args.host or ['mac', 'windows']
    rng = random.Random(args.seed)
    scratch = Path(tempfile.mkdtemp(prefix='pa-demo-', dir='/tmp'))
    env = {k: v for k, v in os.environ.items()
           if not k.startswith(('PMUX', 'PRISMATTYC_', 'XDG_', 'HIVE_'))}
    for directory in ['home', 'config', 'data', 'state', 'runtime']:
        (scratch / directory).mkdir(mode=0o700)
    env.update(HOME=str(scratch / 'home'), SHELL='/bin/sh', TERM='xterm-256color',
               XDG_RUNTIME_DIR=str(scratch / 'runtime'), XDG_CONFIG_HOME=str(scratch / 'config'),
               XDG_DATA_HOME=str(scratch / 'data'), XDG_STATE_HOME=str(scratch / 'state'),
               PRISMATTYC_CONFIG=str(scratch / 'config.toml'), PRISMATTYC_NO_AGENT_SKILLS='1')
    (scratch / 'config.toml').write_text('install_agent_skills = false\n')
    attached = dict(spec.split('=', 1) for spec in args.attach)
    if attached:
        hosts = [label for label in hosts if label in attached] if args.host else list(attached)
    daemons = {label: Daemon(args.bins.resolve(), scratch, label, env, attached.get(label))
               for label in hosts}
    outages = []
    for spec in args.outage:
        label, at, length = spec.split(':')
        outages.append((label, float(at), float(at) + float(length)))
    stopping = False

    def stop(*_):
        nonlocal stopping
        stopping = True

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    manual = {'down': False}
    signal.signal(signal.SIGUSR1, lambda *_: manual.update(down=True))
    signal.signal(signal.SIGUSR2, lambda *_: manual.update(down=False))
    last = hosts[-1]
    try:
        for daemon in daemons.values():
            daemon.start()
        args.sockets.write_text(json.dumps({label: str(d.socket) for label, d in daemons.items()}))
        seats = [Seat(daemons[label], name, profile, rng)
                 for label in hosts for name, profile in SEATS[label]]
        began = time.monotonic()
        while not stopping and (args.seconds == 0 or time.monotonic() - began < args.seconds):
            elapsed = time.monotonic() - began
            for label, start, end in outages:
                down = start <= elapsed < end
                daemon = daemons[label]
                if down and daemon.alive():
                    daemon.stop()
                elif not down and not daemon.alive():
                    daemon.start()
                    for seat in seats:
                        if seat.host is daemon:
                            seat.run = None
            target = daemons[last]
            if manual['down'] and target.alive():
                target.stop()
            elif not manual['down'] and not target.alive() and not any(
                    label == last and start <= elapsed < end for label, start, end in outages):
                target.start()
                for seat in seats:
                    if seat.host is target:
                        seat.run = None
            now = time.monotonic()
            for seat in seats:
                if seat.host.alive():
                    seat.step(now)
            time.sleep(0.05)
    finally:
        for daemon in daemons.values():
            daemon.stop()
        shutil.rmtree(scratch, ignore_errors=True)
        args.sockets.unlink(missing_ok=True)


if __name__ == '__main__':
    sys.exit(main())
