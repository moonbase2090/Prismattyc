#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Drive the contained monitor's Unix terminal input through a private real PTY."""
import argparse
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bins', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    args = parser.parse_args()
    if os.name != 'posix':
        parser.error('Requires a Unix PTY; Windows uses native console input.')
    def interrupted(signum, _frame):
        raise SystemExit(128 + signum)
    for name in ('SIGTERM', 'SIGHUP'):
        signal.signal(getattr(signal, name), interrupted)
    bins, out = args.bins.resolve(), args.out.resolve()
    out.mkdir(parents=True, mode=0o700, exist_ok=False)
    runtime = Path(tempfile.mkdtemp(prefix='pinput-', dir='/tmp'))
    env = {k: v for k, v in os.environ.items()
           if not k.startswith(('PMUX', 'PRISMATTYC_', 'XDG_', 'HIVE_'))}
    for directory in ('home', 'config', 'data', 'state', 'runtime'):
        (runtime / directory).mkdir(mode=0o700)
    endpoint = runtime / 'runtime/daemon.sock'
    config = runtime / 'config.toml'
    config.write_text('start_at_login = false\n[mux]\nexperimental_activity = true\n')
    env.update(HOME=str(runtime / 'home'), SHELL='/bin/sh', TERM='xterm-256color',
               PATH=str(bins) + ':/usr/bin:/bin:/usr/sbin:/sbin',
               PMUX_SOCKET=str(endpoint), PRISMATTYC_CONFIG=str(config),
               PMUX_SESSION_AGENTS=str(runtime / 'agents.json'), PRISMATTYC_DEFAULT_PLUGINS='0',
               XDG_CONFIG_HOME=str(runtime / 'config'), XDG_DATA_HOME=str(runtime / 'data'),
               XDG_STATE_HOME=str(runtime / 'state'), XDG_RUNTIME_DIR=str(runtime / 'runtime'))
    daemon = stream = reader = None
    result = {'status': 'FAIL', 'writes': []}
    sequence = 0

    def cli(*arguments):
        command = [str(bins / 'pmux'), '--socket', str(endpoint), *map(str, arguments)]
        process = subprocess.run(command, env=env, capture_output=True, text=True, timeout=45)
        with (out / 'commands.jsonl').open('a') as log:
            log.write(json.dumps({'arguments': arguments, 'exit': process.returncode,
                                  'stdout': process.stdout, 'stderr': process.stderr}) + '\n')
        process.check_returncode()
        return process.stdout

    def request(kind, **fields):
        nonlocal sequence
        sequence += 1
        stream.sendall((json.dumps({'version': 1, 'request_id': sequence, 'type': kind,
                                   **fields}) + '\n').encode())
        reply = json.loads(reader.readline(4 * 1024 * 1024))
        assert reply['request_id'] == sequence and reply['status'] == 'ok', reply
        return reply['response']

    def wait(probe, label, timeout=30):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            assert daemon.poll() is None, 'private daemon exited'
            value = probe()
            if value:
                return value
            time.sleep(.01)
        raise AssertionError('timed out: ' + label)

    def doctor():
        assert daemon.poll() is None and endpoint.is_socket()
        report = {'daemon_alive': True, 'private_socket': True,
                  'version': cli('--version').strip(), 'sessions': cli('ls')}
        with (out / 'doctor.jsonl').open('a') as log:
            log.write(json.dumps(report) + '\n')

    try:
        with (out / 'daemon.log').open('w') as log:
            daemon = subprocess.Popen([str(bins / 'pmuxd'), '--socket', str(endpoint),
                                       '--', '/bin/sh', '-c', 'read -r line'],
                                      env=env, cwd=runtime, stdout=log, stderr=log)
        wait(lambda: endpoint.exists(), 'private daemon')
        stream = socket.socket(socket.AF_UNIX)
        stream.settimeout(5)
        stream.connect(str(endpoint))
        reader = stream.makefile('rb')
        doctor()
        cli('plugin', 'install', 'org.moonbase.activity-monitor')
        cli('plugin', 'enable', 'org.moonbase.activity-monitor', '--allow', 'activity.read:local')
        client = request('register_client')['client_id']
        for seat in ('000-alpha', '000-beta'):
            request('publish_activity', event={'schema_version': 1, 'seat': seat,
                    'run_id': 'input-proof', 'event_id': seat, 'source': 'hooks', 'kind': 'waiting'})
        driver = runtime / 'driver.py'
        modes = out / 'terminal-modes.json'
        driver.write_text('''import json, subprocess, sys, termios
from pathlib import Path
def modes():
    value = termios.tcgetattr(0)
    if sys.platform == "darwin":
        value[3] &= ~termios.PENDIN
    return repr(value)
before = modes()
child = subprocess.run(sys.argv[2:])
after = modes()
Path(sys.argv[1]).write_text(json.dumps({"before":before,"after":after,"exit":child.returncode}))
print("INPUT_VIEW_RESTORED" if before == after and child.returncode == 0 else "INPUT_MODE_LEAK", flush=True)
sys.stdin.readline()
''')
        cli('new', '--no-attach', '--no-agent', 'input-view', '--', sys.executable,
            str(driver), str(modes), str(bins / 'pmux'), '--socket', str(endpoint),
            'plugin', 'run', 'org.moonbase.activity-monitor', '--no-pixels')
        session = next(s for s in request('snapshot')['snapshot']['sessions'] if s['name'] == 'input-view')
        window = session['windows'][0]
        pane = window['panes'][0]['id']
        request('resize', window_id=window['id'], cols=100, rows=30)
        request('takeover_lease', client_id=client, pane_id=pane)

        def send(data):
            started = time.monotonic()
            request('write_pane', client_id=client, pane_id=pane, data=data)
            result['writes'].append({'bytes_hex': data.encode().hex(), 'monotonic': started})
            return started

        def screen(name, contains=(), absent=(), timeout=30):
            def observe():
                text = cli('save-buffer', pane, '-')
                return text if all(s in text for s in contains) and all(s not in text for s in absent) else None
            text = wait(observe, name, timeout)
            (out / (name + '.txt')).write_text(text)
            return text

        screen('initial', ('000-alpha', '000-beta'))
        send('gjd')
        screen('beta-details', ('← 000-beta', 'Harness'))
        started = send('\x1b')
        screen('lone-escape-list', ('000-alpha', '000-beta'), ('Harness',), timeout=.5)
        result['escape_to_list_ms'] = (time.monotonic() - started) * 1000
        assert result['escape_to_list_ms'] < 500, result
        # Separate control writes reach the real PTY. Keep the gap below the
        # parser deadline, and retain the actual schedule rather than assuming it.
        started = send('\x1b')
        time.sleep(.01)
        continued = send('[A')
        result['split_gap_ms'] = (continued - started) * 1000
        assert result['split_gap_ms'] < 40, result
        time.sleep(.1)
        send('d')
        screen('split-up-alpha-details', ('← 000-alpha', 'Harness'), ('← 000-beta',))
        send('\x1b')
        screen('details-return', ('000-alpha', '000-beta'), ('Harness',), timeout=.5)
        send('?')
        screen('help-open', ('Keys',))
        send('\x1b')
        screen('help-dismissed', ('000-alpha', '000-beta'), ('Keys',), timeout=.5)
        send('q')
        screen('restored', ('INPUT_VIEW_RESTORED',), ('INPUT_MODE_LEAK',))
        observed = json.loads(modes.read_text())
        assert observed['before'] == observed['after'] and observed['exit'] == 0, observed
        doctor()
        result.update(status='PASS', split_up='000-beta to 000-alpha', terminal_modes_restored=True)
    except BaseException as error:
        result['error'] = repr(error)
        if daemon is not None and daemon.poll() is None:
            try:
                doctor()
            except Exception as diagnostic:
                result['doctor_error'] = repr(diagnostic)
        raise
    finally:
        if reader is not None:
            reader.close()
        if stream is not None:
            stream.close()
        if daemon is not None:
            if daemon.poll() is None:
                daemon.terminate()
                try:
                    daemon.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    daemon.kill()
                    daemon.wait(timeout=5)
            result['daemon_reaped'] = daemon.poll() is not None
        shutil.rmtree(runtime)
        result['scratch_removed'] = not runtime.exists()
        (out / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps(result))


if __name__ == '__main__':
    main()
