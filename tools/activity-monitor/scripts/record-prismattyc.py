#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Run the monitor in a real Prismattyc pane and keep the host's presented frames.

macOS only, in a logged-in desktop session. The script starts a private pmuxd and a private
prismattyc-host with PRISMATTYC_DUMP_PRESENT, types the monitor command into a pane, and copies
every presented frame to OUT/frames with its time. Frames are the host framebuffer (what
Prismattyc drew, including Kitty graphics it decoded), not an OS screenshot.

Steps in --plan (JSON list, times in seconds after the monitor starts):
  {"at": 2, "keys": "j"}                 pane-write keys ("\\r" Enter, "\\t" Tab, "\\u001b" Esc)
  {"at": 5, "split": "h"}                join a new shell pane beside the monitor (h or v)
  {"at": 8, "unsplit": true}             move the most recent split pane out again
  {"at": 9, "signal": [PID, "USR1"]}     signal another process (for example, the demo feed)
  {"at": 12, "end": true}

  record-prismattyc.py --bins target/debug --plan plan.json --out DIR -- MONITOR ARGS...
"""

import argparse
import json
import os
import shlex
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--bins', required=True, type=Path)
    parser.add_argument('--plan', required=True, type=Path)
    parser.add_argument('--out', required=True, type=Path)
    parser.add_argument('--font-px', type=float, default=15.0)
    parser.add_argument('command', nargs=argparse.REMAINDER)
    args = parser.parse_args()
    assert sys.platform == 'darwin', 'run on macOS in a logged-in desktop session'
    command = args.command[1:] if args.command[:1] == ['--'] else args.command
    plan = json.loads(args.plan.read_text())
    bins, out = args.bins.resolve(), args.out.resolve()
    out.mkdir(parents=True, exist_ok=False)
    frames = out / 'frames'
    frames.mkdir()
    runtime = Path(tempfile.mkdtemp(prefix='pam-', dir='/tmp'))
    home = out / 'home'
    home.mkdir()
    env = {k: v for k, v in os.environ.items() if not k.startswith(('PMUX', 'PRISMATTYC_', 'XDG_'))}
    env.update(HOME=str(home), PATH=str(bins) + ':/usr/bin:/bin:/usr/sbin:/sbin', SHELL='/bin/bash',
               PS1='$ ', PMUX_SOCKET=str(runtime / 'pmux.sock'), XDG_RUNTIME_DIR=str(runtime),
               XDG_CONFIG_HOME=str(out / 'config'), XDG_DATA_HOME=str(out / 'data'),
               XDG_STATE_HOME=str(out / 'state'), PRISMATTYC_DUMP_PRESENT=str(out / 'present.png'))
    config = out / 'config.toml'
    config.write_text(f'font_px = {args.font_px}\nwindow_opacity = 1.0\nspace_rail = "off"\n'
                      'install_agent_skills = false\n')
    env['PRISMATTYC_CONFIG'] = str(config)
    env['PRISMATTYC_NO_AGENT_SKILLS'] = '1'
    commands = (out / 'commands.jsonl').open('w')
    daemon = host = None

    def cli(*argv, check=True):
        done = subprocess.run([str(bins / 'pmux'), *map(str, argv)], env=env, capture_output=True,
                              text=True, timeout=30)
        commands.write(json.dumps({'args': argv, 'exit': done.returncode, 'stdout': done.stdout[-2000:],
                                   'stderr': done.stderr[-2000:]}) + '\n')
        commands.flush()
        if check and done.returncode:
            raise SystemExit(f'pmux {argv} failed: {done.stderr}')
        return done.stdout

    def snapshot():
        with socket.socket(socket.AF_UNIX) as stream:
            stream.settimeout(5)
            stream.connect(env['PMUX_SOCKET'])
            stream.sendall((json.dumps({'version': 1, 'request_id': 1, 'type': 'snapshot'}) + '\n').encode())
            return json.loads(stream.makefile().readline())['response']['snapshot']

    def wait(probe, label, timeout=30):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            try:
                value = probe()
                if value:
                    return value
            except (OSError, ValueError, KeyError, subprocess.CalledProcessError):
                pass
            time.sleep(0.1)
        raise SystemExit(f'timed out: {label}')

    def session(name):
        return next(s for s in snapshot()['sessions'] if s['name'].startswith(name))

    seen = None
    index = 0
    began = None

    def collect():
        nonlocal seen, index
        try:
            meta = (out / 'present.json').read_bytes()
            png = (out / 'present.png').read_bytes()
            again = (out / 'present.json').read_bytes()
        except FileNotFoundError:
            return
        if meta != again or meta == seen:
            return
        seen = meta
        stamp = time.monotonic() - began if began else 0.0
        (frames / f'{index:05d}.png').write_bytes(png)
        with (out / 'frames.jsonl').open('a') as log:
            log.write(json.dumps({'index': index, 't': round(stamp, 3), 'meta': json.loads(meta)}) + '\n')
        index += 1

    splits = []
    try:
        log = (out / 'daemon.log').open('w')
        daemon = subprocess.Popen([str(bins / 'pmuxd'), '--socket', env['PMUX_SOCKET'], '--', '/bin/bash',
                                   '--noprofile', '--norc'], env=env, stdout=log, stderr=log)
        wait(lambda: Path(env['PMUX_SOCKET']).exists(), 'private daemon')
        cli('space', 'create', 'agents', '--no-attach')
        monitor = session('agents')
        pane = monitor['windows'][0]['panes'][0]['id']
        host_log = (out / 'host.log').open('w')
        host = subprocess.Popen([str(bins / 'prismattyc-host'), '--no-splash', '--attach-session',
                                 str(monitor['id'])], env=env, stdout=host_log, stderr=host_log)
        wait(lambda: json.loads(cli('render-status', '--json', check=False) or '{}').get('host_pid') == host.pid,
             'host render status')
        time.sleep(1.5)
        cli('pane-write', pane, '--text', 'clear; exec ' + shlex.join(command), '--submit', 'enter')
        began = time.monotonic()
        pending = list(plan)
        while pending:
            now = time.monotonic() - began
            if pending[0]['at'] <= now:
                step = pending.pop(0)
                if 'keys' in step:
                    # The monitor redraws continuously, so pane-write sees it as busy.
                    cli('send', pane, step['keys'], '--literal', '--force')
                if 'split' in step:
                    name = f'shell{len(splits) + 1}'
                    cli('new', '--no-attach', '--no-agent', name)
                    extra = session(name)['windows'][0]['panes'][0]['id']
                    tab = session('agents')['windows'][0]['id']
                    cli('join-pane', extra, '--to', tab, '-' + step['split'])
                    splits.append(extra)
                if step.get('unsplit') and splits:
                    cli('break-pane', splits.pop())
                if 'signal' in step:
                    target, name = step['signal']
                    os.kill(int(target), getattr(signal, f'SIG{name}'))
                if step.get('end'):
                    break
                continue
            collect()
            time.sleep(0.015)
        (out / 'result.json').write_text(json.dumps({'status': 'DONE', 'frames': index,
                                                      'host_pid': host.pid}, indent=2))
    finally:
        for process in (host, daemon):
            if process and process.poll() is None:
                process.terminate()
                try:
                    process.wait(5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
        shutil.rmtree(runtime, ignore_errors=True)
        shutil.rmtree(home, ignore_errors=True)


if __name__ == '__main__':
    sys.exit(main())
