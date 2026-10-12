#!/usr/bin/env python3
"""Exercise real pmux sessions, mail, or saved spaces with a private daemon."""
import argparse
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import tempfile
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bins', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--feature', choices=['sessions-mail', 'spaces'], required=True)
    args = parser.parse_args()
    if os.name != 'posix':
        parser.error('This helper requires Unix; use the Windows feature recipe on Windows.')
    bins, out = args.bins.resolve(), args.out.resolve()
    out.mkdir(parents=True, mode=0o700, exist_ok=False)
    scratch = Path(tempfile.mkdtemp(prefix='pv-', dir='/tmp'))
    daemon = None
    result = {'feature': args.feature, 'status': 'FAIL'}
    env = {k: v for k, v in os.environ.items()
           if not k.startswith(('PMUX', 'PRISMATTYC_', 'XDG_'))}
    for directory in ['home', 'config', 'data', 'state', 'runtime']:
        (scratch / directory).mkdir(mode=0o700)
    shell = scratch / 'shell'
    shell.write_text('#!/bin/sh\nexec /bin/sh\n')
    shell.chmod(0o700)
    env.update(HOME=str(scratch / 'home'), SHELL=str(shell), PS1='verify> ',
               PATH=str(bins) + ':/usr/bin:/bin:/usr/sbin:/sbin',
               PMUX_SOCKET=str(scratch / 'runtime/pmux.sock'),
               XDG_RUNTIME_DIR=str(scratch / 'runtime'),
               XDG_CONFIG_HOME=str(scratch / 'config'),
               XDG_DATA_HOME=str(scratch / 'data'),
               XDG_STATE_HOME=str(scratch / 'state'),
               PRISMATTYC_CONFIG=str(scratch / 'config.toml'),
               PRISMATTYC_NO_AGENT_SKILLS='1')
    (scratch / 'config.toml').write_text('install_agent_skills = false\n')

    def cli(*arguments):
        process = subprocess.run([str(bins / 'pmux'), *map(str, arguments)],
                                 env=env, cwd=scratch, capture_output=True, text=True, timeout=15)
        with (out / 'commands.jsonl').open('a') as log:
            log.write(json.dumps({'args': arguments, 'stdout': process.stdout,
                                  'stderr': process.stderr, 'exit': process.returncode}) + '\n')
        process.check_returncode()
        return process.stdout

    def snapshot():
        with socket.socket(socket.AF_UNIX) as stream:
            stream.settimeout(5)
            stream.connect(env['PMUX_SOCKET'])
            stream.sendall(b'{"version":1,"request_id":1,"type":"snapshot"}\n')
            return json.loads(stream.makefile().readline())['response']['snapshot']

    def wait(probe):
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            assert daemon.poll() is None, 'private daemon exited'
            value = probe()
            if value:
                return value
            time.sleep(.1)
        raise AssertionError('timed out waiting for observable state')

    def doctor():
        assert daemon.poll() is None, 'private daemon exited'
        assert Path(env['PMUX_SOCKET']).is_socket()
        report = {'version': cli('--version').strip(), 'sessions': cli('ls'),
                  'private_socket': True, 'daemon_alive': True}
        with (out / 'doctor.jsonl').open('a') as log:
            log.write(json.dumps(report) + '\n')

    try:
        with (out / 'daemon.log').open('w') as log:
            daemon = subprocess.Popen([str(bins / 'pmuxd'), '--socket', env['PMUX_SOCKET'],
                                       '--', '/bin/sh'], env=env, cwd=scratch, stdout=log, stderr=log)
        wait(lambda: Path(env['PMUX_SOCKET']).exists())
        doctor()
        cli('space', 'create', 'verify', '--session-name', 'verifier', '--no-attach')
        before = next(s for s in snapshot()['sessions'] if s['name'] == 'verifier')
        pane = before['windows'][0]['panes'][0]
        cli('send', pane['id'], "printf 'VERIFY_%s_OK\\n' LIVE", '--enter')
        output = wait(lambda: (s if 'VERIFY_LIVE_OK' in (s := cli('save-buffer', 'verifier', '-')) else None))
        (out / 'pane.txt').write_text(output)
        if args.feature == 'sessions-mail':
            cli('mail', '--as', 'verifier', 'send', 'verifier', '--summary', 'verification', '--body', 'literal-mail-proof')
            letters = json.loads(cli('mail', '--as', 'verifier', 'claim', '--json'))['letters']
            assert len(letters) == 1 and letters[0]['body'] == 'literal-mail-proof'
            cli('mail', '--as', 'verifier', 'commit', letters[0]['id'])
            assert json.loads(cli('mail', '--as', 'verifier', 'claim', '--json'))['letters'] == []
        else:
            cli('space', 'save', 'verify')
            cli('space', 'open', 'verify', '--no-attach', '--no-run')
            cli('space', 'rename', 'verify', 'renamed')
            assert 'renamed' in cli('space', 'ls')
            definitions = list((scratch / 'data/prismattyc/spaces').rglob('renamed.json'))
            assert len(definitions) == 1, 'renamed saved definition missing or ambiguous'
            saved = out / 'saved-spaces'
            saved.mkdir()
            for path in definitions:
                shutil.copyfile(path, saved / path.name)
            cli('space', 'rm', 'renamed')
            assert not definitions[0].exists(), 'saved definition survived removal'
        after = next(s for s in snapshot()['sessions'] if s['id'] == before['id'])
        assert after['windows'][0]['panes'][0]['child_pid'] == pane['child_pid']
        assert 'VERIFY_LIVE_OK' in cli('save-buffer', 'verifier', '-')
        doctor()
        result.update(status='PASS', session_and_child='preserved', pane_output='VERIFY_LIVE_OK')
    except Exception as error:
        result['error'] = str(error)
        if daemon is not None and daemon.poll() is None:
            try:
                doctor()
            except Exception as diagnostic:
                result['doctor_error'] = str(diagnostic)
        raise
    finally:
        if daemon is not None:
            if daemon.poll() is None:
                daemon.terminate()
                try:
                    daemon.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    daemon.kill()
                    daemon.wait(timeout=5)
            result['daemon_reaped'] = daemon.poll() is not None
        shutil.rmtree(scratch)
        result['scratch_removed'] = not scratch.exists()
        (out / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
        assert (out / 'commands.jsonl').exists(), 'command evidence missing after cleanup'
    print(json.dumps(result))


if __name__ == '__main__':
    main()
