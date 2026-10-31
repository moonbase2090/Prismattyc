#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Drive a private daemon, metadata adapter, and live terminal activity viewer."""
import argparse
import json
import os
from pathlib import Path
import shlex
import select
import shutil
import socket
import subprocess
import tempfile
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bins', required=True, type=Path)
    parser.add_argument('--monitor', required=True, type=Path)
    parser.add_argument('--out', required=True, type=Path)
    parser.add_argument('--producer-command', type=Path,
                        help='Optional vetted JSON argv with {socket}, {emitter}, {seat} placeholders')
    parser.add_argument('--producer-source', choices=['hooks', 'notify'])
    args = parser.parse_args()
    bins, monitor, out = args.bins.resolve(), args.monitor.resolve(), args.out.resolve()
    out.mkdir(parents=True, mode=0o700, exist_ok=False)
    scratch = Path(tempfile.mkdtemp(prefix='pa-', dir='/tmp'))
    emitter = bins / 'examples/activity-emit'
    env = {k: v for k, v in os.environ.items()
           if not k.startswith(('PMUX', 'PRISMATTYC_', 'XDG_', 'HIVE_'))}
    for directory in ['home', 'config', 'data', 'state', 'runtime']:
        (scratch / directory).mkdir(mode=0o700)
    env.update(HOME=str(scratch / 'home'), SHELL='/bin/sh', PS1='verify> ',
               TERM='xterm-256color', XDG_RUNTIME_DIR=str(scratch / 'runtime'),
               XDG_CONFIG_HOME=str(scratch / 'config'), XDG_DATA_HOME=str(scratch / 'data'),
               XDG_STATE_HOME=str(scratch / 'state'),
               PRISMATTYC_CONFIG=str(scratch / 'config.toml'), PRISMATTYC_NO_AGENT_SKILLS='1')
    (scratch / 'config.toml').write_text('install_agent_skills = false\n')
    endpoint = scratch / 'runtime/feed.sock'
    daemon = tail = None
    direct_processes = []
    monitor_pane = None
    checks = []
    result = {'status': 'FAIL', 'checks': checks}

    def wait(probe, seconds=15):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            if daemon is not None:
                assert daemon.poll() is None, 'private daemon exited'
            value = probe()
            if value:
                return value
            time.sleep(.05)
        raise AssertionError('timed out waiting for observable activity')

    def request(kind, **fields):
        wire = {'type': kind, 'version': 1, 'request_id': 1, **fields}
        with socket.socket(socket.AF_UNIX) as stream:
            stream.settimeout(3)
            stream.connect(str(endpoint))
            stream.sendall(json.dumps(wire).encode() + b'\n')
            response = json.loads(stream.makefile('rb').readline(4 * 1024 * 1024))
        assert response['version'] == 1 and response['request_id'] == 1
        return response

    def read(cursor=None, limit=256):
        reply = request('read_activity', cursor=cursor, limit=limit)
        assert reply['status'] == 'ok', reply
        return reply['response']['page']

    def event(number, seat='worker-a', kind='working'):
        return {'schema_version': 1, 'seat': seat, 'run_id': 'run-1',
                'event_id': f'event-{number}', 'source': 'runtime', 'kind': kind}

    def publish(value):
        reply = request('publish_activity', event=value)
        assert reply['status'] == 'ok', reply
        return reply['response']['ack']

    def cli(*arguments):
        cli_env = dict(env, PMUX_SOCKET=str(endpoint))
        completed = subprocess.run([str(bins / 'pmux'), *map(str, arguments)],
                                   env=cli_env, cwd=scratch, capture_output=True,
                                   text=True, timeout=5, check=True)
        return completed.stdout

    def screen():
        return cli('save-buffer', monitor_pane, '-')

    def identities(snapshot):
        return [(s['id'], w['id'], p['id'], p['child_pid'])
                for s in snapshot['sessions'] for w in s['windows'] for p in w['panes']]

    def start(enabled):
        nonlocal daemon
        command = [str(bins / 'pmuxd'), '--socket', str(endpoint), '--cols', '120', '--rows', '24']
        if enabled:
            command.append('--experimental-activity')
        command += ['--', '/bin/sh']
        with (out / 'daemon.log').open('ab') as log:
            daemon = subprocess.Popen(command, cwd=scratch, env=env, stdout=log, stderr=log)
            direct_processes.append(daemon)
        wait(lambda: endpoint.exists())
        assert request('ping')['status'] == 'ok'

    def stop():
        nonlocal daemon
        if daemon is not None:
            daemon.terminate()
            try:
                daemon.wait(timeout=5)
            except subprocess.TimeoutExpired:
                daemon.kill()
                daemon.wait(timeout=5)
            daemon = None
            # SIGTERM can leave this driver's private socket inode behind.
            endpoint.unlink(missing_ok=True)

    try:
        start(False)
        for reply in [request('publish_activity', event=event(0)),
                      request('read_activity', cursor=None, limit=10)]:
            assert reply['status'] == 'error' and reply['error']['code'] == 'activity_disabled'
        checks.append('disabled by default')
        stop()
        start(True)
        snapshot = request('snapshot')['response']['snapshot']
        monitor_pane = snapshot['sessions'][0]['windows'][0]['panes'][0]['id']
        command = shlex.join([str(monitor), '--socket', str(endpoint)])
        cli('send', monitor_pane, command, '--enter')
        wait(lambda: 'Activity monitor' in screen())
        before = request('snapshot')['response']['snapshot']
        hook = {'hook_event_name': 'PreToolUse', 'session_id': 'demo-session',
                'tool_use_id': 'demo-call', 'tool_name': 'Bash',
                'tool_input': {'command': 'do-not-retain-this-command'}}
        emitted = subprocess.run([str(emitter), '--socket', str(endpoint), '--seat',
                                  'worker-live', '--hook'], input=json.dumps(hook),
                                 capture_output=True, text=True, env=env, timeout=5)
        assert emitted.returncode == 0 and not emitted.stdout and not emitted.stderr
        live = wait(lambda: (text if 'worker-live' in (text := screen()) else None))
        assert 'command_started' in live
        (out / 'live-terminal.txt').write_text(live)
        checks.append('live viewer changes after hook event without keyboard input')
        assert read()['records'][0]['event']['kind'] == 'command_started'
        assert 'do-not-retain' not in json.dumps(read())
        checks.append('adapter omits command arguments')

        if args.producer_command:
            substitutions = {'socket': str(endpoint), 'emitter': str(emitter), 'seat': 'worker-runner'}
            command = json.loads(args.producer_command.read_text())
            for key, value in substitutions.items():
                command = [part.replace('{' + key + '}', value) for part in command]
            # Use ambient runner credentials, with a private socket and working directory.
            runner_env = {k: v for k, v in os.environ.items()
                          if not k.startswith(('PMUX', 'PRISMATTYC_', 'HIVE_'))}
            completed = subprocess.run(command, env=runner_env, cwd=scratch,
                                       capture_output=True, timeout=120)
            assert completed.returncode == 0, 'external producer failed'
            observed = wait(lambda: [r for r in read()['records']
                                    if r['event']['seat'] == 'worker-runner'])
            sources = sorted({r['event']['source'] for r in observed})
            assert any(source in ('notify', 'hooks') for source in sources)
            if args.producer_source:
                assert args.producer_source in sources
            result['producer_sources'] = sources
            checks.append('real external producer emitted an observed event')

        # Keyboard input to the viewer begins only after the live assertion.
        cli('send', monitor_pane, 'q')
        wait(lambda: 'Activity monitor' not in screen())
        result['viewer_exit_observed'] = True
        checks.append('viewer exits and restores terminal')
        snap = subprocess.run([str(monitor), '--socket', str(endpoint), '--snapshot'],
                              capture_output=True, text=True, env=env, timeout=5, check=True)
        assert 'worker-live' in snap.stdout and 'command_started' in snap.stdout
        (out / 'viewer.txt').write_text(snap.stdout)

        ack = publish(event(1))
        duplicate = publish(event(1))
        assert duplicate['duplicate'] is True and duplicate['cursor'] == ack['cursor']
        conflict = event(1, kind='waiting')
        assert request('publish_activity', event=conflict)['status'] == 'error'
        checks.append('bounded retry dedup and conflicting retry rejection')
        publish(event(2, 'worker-b'))
        listed = subprocess.run([str(monitor), '--socket', str(endpoint), '--seats'],
                                capture_output=True, text=True, env=env, timeout=5, check=True)
        seats = json.loads(listed.stdout)
        assert seats['retained_history_only'] is True
        assert {'worker-live', 'worker-a', 'worker-b'} <= {s['seat'] for s in seats['seats']}
        recent = subprocess.run([str(monitor), '--socket', str(endpoint), '--recent', 'worker-b'],
                                capture_output=True, text=True, env=env, timeout=5, check=True)
        records = json.loads(recent.stdout)['records']
        assert len(records) == 1 and records[0]['event']['event_id'] == 'event-2'
        (out / 'connector-seats.json').write_text(listed.stdout)
        (out / 'connector-recent.json').write_text(recent.stdout)
        checks.append('JSON connector lists observed seats and filters recent events')
        tail = subprocess.Popen([str(monitor), '--socket', str(endpoint), '--tail',
                                 '--seat', 'worker-tail'], env=env, cwd=scratch,
                                stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                stderr=subprocess.PIPE)
        direct_processes.append(tail)
        tail_bytes = bytearray()

        def tail_observed():
            if select.select([tail.stdout], [], [], .1)[0]:
                tail_bytes.extend(os.read(tail.stdout.fileno(), 65536))
            for line in tail_bytes.splitlines():
                try:
                    value = json.loads(line)
                except ValueError:
                    continue
                if value.get('type') == 'activity':
                    for record in value['page']['records']:
                        assert record['event']['seat'] == 'worker-tail'
                        if record['event']['event_id'] == 'event-3':
                            return True
            return False

        publish(event(3, 'worker-tail', 'waiting'))
        wait(tail_observed)
        tail.terminate()
        tail.wait(timeout=5)
        assert tail.returncode == 0
        (out / 'connector-tail.jsonl').write_bytes(tail_bytes)
        checks.append('filtered JSON tail streams new events and exits on signal')
        page = read(limit=1)
        assert len(page['records']) == 1 and page['has_more'] is True
        next_page = read(page['cursor'], limit=1)
        assert next_page['records'][0]['sequence'] == page['records'][0]['sequence'] + 1
        checks.append('pagination resumes from last delivered event')
        assert request('read_activity', cursor=None, limit=0)['status'] == 'error'
        invalid = event(3)
        invalid['run_id'] = '\u001b[2J'
        assert request('publish_activity', event=invalid)['status'] == 'error'
        checks.append('invalid identifiers and read limits rejected')

        first_cursor = page['cursor']
        for number in range(10, 525):
            publish(event(number))
        retained = read(first_cursor)
        assert retained['reset'] == 'retention_gap'
        assert retained['oldest_sequence'] > first_cursor['sequence'] + 1
        assert len(retained['records']) == 256
        boundary = {'epoch': retained['epoch'], 'sequence': retained['oldest_sequence'] - 1}
        assert read(boundary, limit=1)['reset'] is None
        checks.append('retention gaps explicit and oldest-minus-one cursor valid')
        after = request('snapshot')['response']['snapshot']
        assert identities(after) == identities(before), 'headless activity changed pane/process identity'
        checks.append('headless events preserve topology and shell identity')
        old_cursor = retained['cursor']
        stop()
        start(True)
        restarted = read(old_cursor)
        assert restarted['reset'] == 'restarted' and restarted['records'] == []
        assert restarted['epoch'] != old_cursor['epoch']
        checks.append('daemon restart changes epoch and resets history')
        result['status'] = 'PASS'
    except Exception as error:
        result['error'] = str(error)
        raise
    finally:
        if tail is not None and tail.poll() is None:
            tail.terminate()
            tail.wait(timeout=5)
        if daemon is not None and daemon.poll() is None and monitor_pane is not None:
            try:
                cli('send', monitor_pane, 'q')
            except (OSError, subprocess.SubprocessError):
                pass
        stop()
        result['direct_processes_reaped'] = all(p.poll() is not None for p in direct_processes)
        shutil.rmtree(scratch)
        result['scratch_removed'] = not scratch.exists()
        (out / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps(result))


if __name__ == '__main__':
    main()
