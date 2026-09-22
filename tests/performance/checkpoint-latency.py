#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Measure private daemon control latency with full history and checkpointing."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import socket
import statistics
import subprocess
import tempfile
import time

p = argparse.ArgumentParser(description=__doc__)
p.add_argument('binary')
p.add_argument('--seconds', type=float, default=12)
p.add_argument('--persist', choices=('on', 'off'), default='on')
p.add_argument('--panes', type=int, default=1)
p.add_argument('--warmup', type=float, default=8, help='seconds to settle initial history checkpoints')
p.add_argument('--active-panes', type=int, help='panes that keep writing after history fill')
a = p.parse_args()
active_panes = a.panes if a.active_panes is None else a.active_panes
if not 0 <= active_panes <= a.panes:
    p.error('active panes must be between zero and panes')
if a.panes < 1 or a.seconds < 5:
    p.error('use at least one pane and five seconds')
binary_hash = hashlib.sha256(Path(a.binary).read_bytes()).hexdigest()
with tempfile.TemporaryDirectory(prefix='pmux-latency-') as tmp:
    root = Path(tmp)
    sock = root / 'mux.sock'
    checkpoint = root / 'log.json'
    env = {k: v for k, v in os.environ.items() if not k.startswith(('PMUX_', 'PRISMATTYC_'))}
    env.update(XDG_DATA_HOME=tmp, XDG_CONFIG_HOME=tmp,
               PMUX_PANE_LOG=str(checkpoint) if a.persist == 'on' else 'off',
               PMUX_SESSION_AGENTS=str(root / 'agents.json'))
    guest = "import os,time; os.write(1, (b'x'*79+b'\\r\\n')*10000); exec('while True:\\n os.write(1,b\".\")\\n time.sleep(.05)')"
    idle_guest = "import os,time; os.write(1, (b'x'*79+b'\\r\\n')*10000); time.sleep(3600)"
    def counters():
        stat = Path(f'/proc/{proc.pid}/stat').read_text().rsplit(')', 1)[1].split()
        io = dict(line.split(': ') for line in Path(f'/proc/{proc.pid}/io').read_text().splitlines())
        return dict(cpu_seconds=(int(stat[11])+int(stat[12]))/os.sysconf('SC_CLK_TCK'),
                    write_bytes=int(io['write_bytes']), wchar=int(io['wchar']))
    proc = subprocess.Popen([a.binary, '--socket', str(sock), '--', 'python3', '-c', guest if active_panes else idle_guest],
                            env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        deadline = time.monotonic() + 10
        while not sock.exists():
            if proc.poll() is not None or time.monotonic() > deadline:
                raise RuntimeError('daemon did not start')
            time.sleep(.01)
        with socket.socket(socket.AF_UNIX) as conn:
            conn.settimeout(5)
            conn.connect(str(sock))
            stream = conn.makefile('rb')
            for i in range(1, a.panes):
                conn.sendall(json.dumps(dict(type='create_session', version=1, request_id=i,
                    name=f'load-{i}', spawn=dict(program='python3', argv=['-c', guest if i < active_panes else idle_guest], cwd=None),
                    cols=80, rows=24)).encode()+b'\n')
                reply = json.loads(stream.readline())
                assert reply['status'] == 'ok', reply
            time.sleep(a.warmup)
            samples = []
            checkpoints = set()
            before = counters()
            started = time.monotonic()
            while time.monotonic() - started < a.seconds:
                t = time.monotonic()
                conn.sendall(json.dumps(dict(type='ping', version=1, request_id=len(samples)+a.panes)).encode()+b'\n')
                reply = json.loads(stream.readline())
                assert reply['status'] == 'ok', reply
                samples.append((time.monotonic()-t)*1000)
                if checkpoint.exists():
                    checkpoints.add(checkpoint.stat().st_mtime_ns)
                time.sleep(.005)
        ordered = sorted(samples)
        if a.persist == 'on':
            assert len(checkpoints) >= (2 if active_panes else 1), 'did not observe expected checkpoints'
        after = counters()
        usage = {key: after[key]-before[key] for key in before}
        print(json.dumps(dict(usage=usage, active_panes=active_panes, binary=a.binary, sha256=binary_hash, persist=a.persist,
            panes=a.panes, checkpoints=len(checkpoints), samples=len(samples),
            median_ms=statistics.median(samples), p99_ms=ordered[int(len(ordered)*.99)],
            max_ms=max(samples), over_50ms=sum(t>50 for t in samples),
            checkpoint_bytes=checkpoint.stat().st_size if checkpoint.exists() else 0)))
    finally:
        proc.kill()
        proc.wait()
