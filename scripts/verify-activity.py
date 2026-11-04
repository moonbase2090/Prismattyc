#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Drive a private daemon, metadata adapter, and live terminal activity viewer."""
import argparse
import json
import os
from pathlib import Path
import queue
import shlex
import shutil
import subprocess
import sys
import tempfile
import threading
import time

from native_control import control_socket



def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bins', required=True, type=Path)
    parser.add_argument('--monitor', required=True, type=Path)
    parser.add_argument('--out', required=True, type=Path)
    parser.add_argument('--producer-command', type=Path,
                        help='Vetted JSON argv with {socket}, {emitter}, {seat}, {hooks}, {project}')
    parser.add_argument('--producer-source', choices=['hooks', 'notify'])
    parser.add_argument('--producer-failure-hook', action='store_true')
    args = parser.parse_args()
    bins, monitor, out = args.bins.resolve(), args.monitor.resolve(), args.out.resolve()
    out.mkdir(parents=True, mode=0o700, exist_ok=False)
    scratch = Path(tempfile.mkdtemp(prefix='pa-'))
    windows = os.name == 'nt'
    suffix = '.exe' if windows else ''
    emitter = bins / ('pmux' + suffix)
    shell = os.environ.get('COMSPEC', 'cmd.exe') if windows else '/bin/sh'
    env = {k: v for k, v in os.environ.items()
           if not k.startswith(('PMUX', 'PRISMATTYC_', 'XDG_', 'HIVE_'))}
    for directory in ['home', 'config', 'data', 'state', 'runtime']:
        (scratch / directory).mkdir(mode=0o700)
    env.update(HOME=str(scratch / 'home'), SHELL=shell, PS1='verify> ',
               TERM='xterm-256color', XDG_RUNTIME_DIR=str(scratch / 'runtime'),
               XDG_CONFIG_HOME=str(scratch / 'config'), XDG_DATA_HOME=str(scratch / 'data'),
               XDG_STATE_HOME=str(scratch / 'state'),
               PRISMATTYC_CONFIG=str(scratch / 'config.toml'), PRISMATTYC_NO_AGENT_SKILLS='1')
    if windows:
        env.update(USERPROFILE=str(scratch / 'home'), APPDATA=str(scratch / 'config'),
                   LOCALAPPDATA=str(scratch / 'data'))
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
        with control_socket(endpoint) as stream:
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
        completed = subprocess.run([str(emitter), *map(str, arguments)],
                                   env=cli_env, cwd=scratch, capture_output=True,
                                   text=True, encoding='utf-8', timeout=5, check=True)
        return completed.stdout

    def emit(seat, *arguments, **options):
        return subprocess.run([str(emitter), '--socket', str(endpoint), 'activity-emit',
                               '--seat', seat, *map(str, arguments)],
                              capture_output=True, text=True, encoding='utf-8', env=env, timeout=8, **options)

    def installed_hooks(file, seat, failure=False):
        arguments = ['--install-hooks', file]
        if failure:
            arguments.append('--failure-hook')
        first = emit(seat, *arguments)
        first.check_returncode()
        before = file.read_bytes()
        second = emit(seat, *arguments)
        second.check_returncode()
        assert second.stdout.strip() == 'unchanged' and file.read_bytes() == before

    def screen():
        return cli('save-buffer', monitor_pane, '-')

    def identities(snapshot):
        return [(s['id'], w['id'], p['id'], p['child_pid'])
                for s in snapshot['sessions'] for w in s['windows'] for p in w['panes']]

    def start(enabled):
        nonlocal daemon
        command = [str(bins / ('pmuxd' + suffix)), '--socket', str(endpoint), '--cols', '120', '--rows', '24']
        if enabled:
            command.append('--experimental-activity')
        command += ['--', shell]
        if windows:
            command += ['/Q', '/D']
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
        if windows:
            # The daemon requires an owner-only, inheritable socket-parent DACL.
            prepare = """$ErrorActionPreference='Stop';
$sid=[Security.Principal.WindowsIdentity]::GetCurrent().User;
$acl=New-Object Security.AccessControl.DirectorySecurity;
$acl.SetOwner($sid); $acl.SetAccessRuleProtection($true,$false);
$rule=New-Object Security.AccessControl.FileSystemAccessRule($sid,'FullControl','ContainerInherit,ObjectInherit','None','Allow');
$acl.AddAccessRule($rule);
Set-Acl -LiteralPath $env:ACTIVITY_PROOF_RUNTIME -AclObject $acl;
"""
            subprocess.run(['powershell.exe', '-NoProfile', '-NonInteractive', '-Command', prepare],
                           env=dict(env, ACTIVITY_PROOF_RUNTIME=str(endpoint.parent)),
                           capture_output=True, timeout=15, check=True)
        config = scratch / 'config.toml'
        default_config = config.read_text()
        config.write_text(default_config + '[mux]\nexperimental_activity = false\n')
        start(False)
        for reply in [request('publish_activity', event=event(0)),
                      request('read_activity', cursor=None, limit=10)]:
            assert reply['status'] == 'error' and reply['error']['code'] == 'activity_disabled'
        ignored = emit('worker-off', '--hook', input=json.dumps({
            'hook_event_name': 'SessionStart', 'session_id': 'disabled-session'}))
        assert ignored.returncode == 0 and ignored.stdout == '' and ignored.stderr == ''
        checks.append('disabled by opt-out')
        stop()
        config.write_text(default_config)
        start(False)
        assert request('read_activity', cursor=None, limit=10)['status'] == 'ok'
        checks.append('enabled by default')
        stop()
        start(True)
        snapshot = request('snapshot')['response']['snapshot']
        monitor_pane = snapshot['sessions'][0]['windows'][0]['panes'][0]['id']
        argv = [str(monitor), '--socket', str(endpoint)]
        command = subprocess.list2cmdline(argv) if windows else shlex.join(argv)
        cli('send', monitor_pane, command, '--literal', '--enter')
        wait(lambda: 'Activity monitor' in screen())
        before = request('snapshot')['response']['snapshot']
        hook = {'hook_event_name': 'PreToolUse', 'session_id': 'demo-session',
                'tool_use_id': 'demo-call', 'tool_name': 'Bash',
                'tool_input': {'command': 'do-not-retain-this-command'}}
        emitted = emit('worker-live', '--hook', input=json.dumps(hook))
        assert emitted.returncode == 0 and not emitted.stdout and not emitted.stderr
        live = wait(lambda: (text if 'worker-live' in (text := screen()) else None))
        assert 'command_started' in live
        (out / 'live-terminal.txt').write_text(live, encoding='utf-8')
        checks.append('live viewer changes after hook event without keyboard input')
        assert read()['records'][0]['event']['kind'] == 'command_started'
        assert 'do-not-retain' not in json.dumps(read())
        checks.append('adapter omits command arguments')

        unfinished = subprocess.Popen([str(emitter), '--socket', str(endpoint), 'activity-emit',
                                       '--seat', 'worker-timeout', '--hook'],
                                      stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                      stderr=subprocess.PIPE, env=env)
        direct_processes.append(unfinished)
        unfinished.wait(timeout=5)
        unfinished.stdin.close()
        assert unfinished.returncode == 0 and unfinished.stdout.read() == b''
        assert unfinished.stderr.read().strip() == b'activity observation unavailable'
        checks.append('unfinished hook input times out without rejecting the observed action')

        settings = scratch / 'installer.json'
        settings.write_text(json.dumps({'hooks': {'SessionStart': [
            {'hooks': [{'type': 'command', 'command': 'echo preserve-existing'}]}]}}))
        installed_hooks(settings, 'worker-installed')
        entry = json.loads(settings.read_text())['hooks']['SessionStart']
        assert len(entry) == 2 and entry[0]['hooks'][0]['command'] == 'echo preserve-existing'
        dispatched = subprocess.run(entry[1]['hooks'][0]['command'], shell=True, env=env,
                                    input=json.dumps({'hook_event_name': 'SessionStart',
                                                      'session_id': 'installed-session'}),
                                    capture_output=True, text=True, encoding='utf-8', timeout=8, check=True)
        assert dispatched.stdout == '' and dispatched.stderr == ''
        assert any(r['event']['seat'] == 'worker-installed' for r in read()['records'])
        removed = emit('worker-installed', '--remove-hooks', settings)
        removed.check_returncode()
        assert json.loads(settings.read_text())['hooks']['SessionStart'] == [
            {'hooks': [{'type': 'command', 'command': 'echo preserve-existing'}]}]
        settings.write_text('{invalid')
        rejected = emit('worker-installed', '--install-hooks', settings)
        assert rejected.returncode != 0 and settings.read_text() == '{invalid'
        checks.append('installed hook command works; repeated install/removal preserves other handlers')

        if args.producer_command:
            hook_file = scratch / 'hooks.json'
            counter = scratch / 'startup-count'
            counter_script = scratch / 'count.py'
            counter_script.write_text('from pathlib import Path\np=Path(' + repr(str(counter)) +
                                      ')\np.write_text(str(int(p.read_text())+1) if p.exists() else "1")\n')
            counter_args = [sys.executable, str(counter_script)]
            counter_command = subprocess.list2cmdline(counter_args) if windows else shlex.join(counter_args)
            hook_file.write_text(json.dumps({'hooks': {'SessionStart': [
                {'hooks': [{'type': 'command', 'command': counter_command, 'timeout': 5}]}]}}))
            installed_hooks(hook_file, 'worker-runner', args.producer_failure_hook)
            substitutions = {'socket': str(endpoint), 'emitter': str(emitter), 'seat': 'worker-runner',
                             'hooks': str(hook_file), 'project': str(scratch)}
            command = json.loads(args.producer_command.read_text())
            for key, value in substitutions.items():
                command = [part.replace('{' + key + '}', value) for part in command]
            # Use ambient runner credentials, with a private socket and working directory.
            runner_env = {k: v for k, v in os.environ.items()
                          if not k.startswith(('PMUX', 'PRISMATTYC_', 'HIVE_'))}
            watermark = read()['current_sequence']
            completed = subprocess.run(command, env=runner_env, cwd=scratch,
                                       capture_output=True, timeout=120)
            assert completed.returncode == 0, 'external producer failed'
            observed = wait(lambda: [r for r in read()['records']
                                    if r['sequence'] > watermark and r['event']['seat'] == 'worker-runner'])
            sources = sorted({r['event']['source'] for r in observed})
            assert any(source in ('notify', 'hooks') for source in sources)
            if args.producer_source:
                assert args.producer_source in sources
            result['producer_sources'] = sources
            assert counter.read_text() == '1', 'preexisting startup hook did not run exactly once'
            emit('worker-runner', '--remove-hooks', hook_file).check_returncode()
            after_first = read()['current_sequence']
            completed = subprocess.run(command, env=runner_env, cwd=scratch,
                                       capture_output=True, timeout=120)
            assert completed.returncode == 0 and counter.read_text() == '2'
            assert read()['current_sequence'] == after_first, 'removed activity hooks still emitted events'
            result['producer_hooks_preserved'] = True
            checks.append('real external producer emitted an observed event')

        # Keyboard input to the viewer begins only after the live assertion.
        cli('send', monitor_pane, 'q')
        wait(lambda: 'Activity monitor' not in screen())
        result['viewer_exit_observed'] = True
        checks.append('viewer exits and restores terminal')
        snap = subprocess.run([str(monitor), '--socket', str(endpoint), '--snapshot'],
                              capture_output=True, text=True, encoding='utf-8', env=env, timeout=5, check=True)
        assert 'worker-live' in snap.stdout and 'command_started' in snap.stdout
        (out / 'viewer.txt').write_text(snap.stdout, encoding='utf-8')

        ack = publish(event(1))
        duplicate = publish(event(1))
        assert duplicate['duplicate'] is True and duplicate['cursor'] == ack['cursor']
        conflict = event(1, kind='waiting')
        assert request('publish_activity', event=conflict)['status'] == 'error'
        checks.append('bounded retry dedup and conflicting retry rejection')
        publish(event(2, 'worker-b'))
        listed = subprocess.run([str(monitor), '--socket', str(endpoint), '--seats'],
                                capture_output=True, text=True, encoding='utf-8', env=env, timeout=5, check=True)
        seats = json.loads(listed.stdout)
        assert seats['retained_history_only'] is True
        assert {'worker-live', 'worker-a', 'worker-b'} <= {s['seat'] for s in seats['seats']}
        recent = subprocess.run([str(monitor), '--socket', str(endpoint), '--recent', 'worker-b'],
                                capture_output=True, text=True, encoding='utf-8', env=env, timeout=5, check=True)
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
        tail_lines = queue.Queue(maxsize=64)

        def read_tail():
            for line in iter(tail.stdout.readline, b''):
                try:
                    tail_lines.put(line, timeout=1)
                except queue.Full:
                    break

        reader = threading.Thread(target=read_tail, daemon=True)
        reader.start()

        def tail_observed():
            while True:
                try:
                    tail_bytes.extend(tail_lines.get_nowait())
                except queue.Empty:
                    break
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
        reader.join(timeout=2)
        assert tail.returncode == (1 if windows else 0)
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
        for child in direct_processes:
            if child.poll() is None:
                child.terminate()
                try:
                    child.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait(timeout=5)
        result['direct_processes_reaped'] = all(p.poll() is not None for p in direct_processes)
        shutil.rmtree(scratch)
        result['scratch_removed'] = not scratch.exists()
        (out / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps(result))


if __name__ == '__main__':
    main()
