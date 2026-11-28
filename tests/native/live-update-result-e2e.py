#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""A live pmux server update, and the result the relaunched app shows.

1. A private pmux server runs two interactive shells. The update flow's own
   restart (`pmux restart --daemon --stop-sessions --consented-config ...
   --worker --json`) hands them to the new server live. The receipt must say
   `updated_live`, and each shell must keep its process.
2. The real app is then launched the way the update relaunches it, with the
   resume record carrying that receipt's sessions, and its result panel is
   captured. The receipt is written to `receipt.json`.

  live-update-result-e2e.py --bins target/debug --out DIR
"""
import argparse
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bins', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    args = parser.parse_args()
    bins, out = args.bins.resolve(), args.out.resolve()
    out.mkdir(parents=True, exist_ok=False)
    runtime = Path(tempfile.mkdtemp(prefix='plive-', dir='/tmp'))
    for name in ('home', 'config', 'data', 'state', 'project'):
        (out / name).mkdir()
    for profile in ('.profile', '.bash_profile', '.bashrc', '.zshrc'):
        (out / 'home' / profile).write_text("PS1='live> '\nPROMPT='live> '\n")
    config = out / 'config.toml'
    config.write_text('splash = false\nstart_at_login = false\nautomatic_update_checks = false\n'
                      'install_agent_skills = false\nspace_startup = "fresh"\nchrome_style = "graphite"\n')
    env = {k: v for k, v in os.environ.items() if not k.startswith(('PMUX', 'PRISMATTYC_', 'XDG_'))}
    env.update(HOME=str(out / 'home'), SHELL='/bin/sh', PS1='live> ',
               PATH=f'{bins}:/usr/bin:/bin:/usr/sbin:/sbin', PMUX=str(bins / 'pmux'),
               PMUX_SERVER=str(bins / 'pmuxd'), PMUX_SOCKET=str(runtime / 'pmux.sock'),
               XDG_RUNTIME_DIR=str(runtime), XDG_DATA_HOME=str(out / 'data'),
               XDG_CONFIG_HOME=str(out / 'config'), XDG_STATE_HOME=str(out / 'state'),
               PRISMATTYC_CONFIG=str(config), PRISMATTYC_NO_AGENT_SKILLS='1', PRISMATTYC_NO_LOGIN_SERVICE='1',
               PRISMATTYC_DUMP_PRESENT=str(out / 'present.png'), PRISMATTYC_E2E_WINDOW_CELLS='120x34')
    result = {'status': 'FAIL', 'platform': sys.platform, 'checks': []}
    host = None

    def cli(*words):
        p = subprocess.run([str(bins / 'pmux'), '--socket', env['PMUX_SOCKET'], *map(str, words)], env=env,
                           cwd=out / 'project', capture_output=True, text=True, timeout=180)
        if p.returncode:
            raise AssertionError(f'pmux {" ".join(map(str, words))} failed: {p.stderr.strip()}')
        return p.stdout

    def wait(probe, label, timeout=60):
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            try:
                value = probe()
                if value:
                    return value
            except (OSError, KeyError, ValueError, IndexError, AssertionError, StopIteration):
                pass
            time.sleep(.25)
        raise AssertionError('timeout: ' + label)

    def sessions_now():
        with socket.socket(socket.AF_UNIX) as client:
            client.settimeout(5)
            client.connect(env['PMUX_SOCKET'])
            client.sendall((json.dumps({'type': 'snapshot', 'version': 1, 'request_id': 1}) + '\n').encode())
            response = json.loads(client.makefile().readline())
        assert response['status'] == 'ok', response
        return response['response']['snapshot']['sessions']

    def pids():
        return {s['name']: s['windows'][0]['panes'][0]['child_pid']
                for s in sessions_now() if s['name'] in ('alpha', 'beta')}

    def record(check):
        result['checks'].append(check)
        print('PASS ' + check, flush=True)

    try:
        cli('up')
        for name in ('alpha', 'beta'):
            cli('new', '--no-attach', name, '--', '/bin/sh', '-i')
        before = wait(lambda: (p := pids()) and len(p) == 2 and p, 'two shells')
        consent = out / 'consent.json'
        cli('session-config', 'save', consent)
        receipt = json.loads(cli('restart', '--daemon', '--stop-sessions', '--consented-config', consent,
                                 '--worker', '--json').strip().splitlines()[-1])
        (out / 'receipt.json').write_text(json.dumps(receipt, indent=2))
        daemon = [c['status'] for c in receipt['components'] if c['component'] == 'daemon']
        assert daemon == ['updated_live'], receipt
        sessions = next(c for c in receipt['components'] if c['component'] == 'sessions')
        assert {'alpha', 'beta'} <= set(sessions['restored']) and not sessions['lost'], sessions
        after = pids()
        assert after == before, f'processes changed: {before} -> {after}'
        record(f'the update handed alpha and beta to the new server live; their shells kept pids {sorted(before.values())}')

        version = json.loads(cli('versions'))['daemon']['version']
        resume = {
            # The line the updater writes; the app adds the sessions line itself.
            'notice': f'Prismattyc {version} is installed. The pmux server moved to {version} '
                      'without stopping; running sessions kept running.',
            'backup': None,
            'restored': {'restored': sessions['restored'], 'lost': sessions['lost'], 'live': True},
        }
        env['PRISMATTYC_COMPLETE_UPDATE'] = json.dumps(resume)
        host = subprocess.Popen([str(bins / 'prismattyc-host')], env=env, cwd=out / 'project',
                                stdout=open(out / 'host.log', 'w'), stderr=subprocess.STDOUT,
                                start_new_session=True)
        wait(lambda: (out / 'present.png').exists(), 'first frame')
        time.sleep(4)
        shutil.copy(out / 'present.png', out / 'live-result.png')
        assert host.poll() is None, 'the app exited'
        record('the relaunched app showed the update result')
        result['status'] = 'PASS'
    finally:
        (out / 'result.json').write_text(json.dumps(result, indent=2))
        if host and host.poll() is None:
            host.terminate()
            try:
                host.wait(10)
            except subprocess.TimeoutExpired:
                host.kill()
        subprocess.run([str(bins / 'pmux'), '--socket', env['PMUX_SOCKET'], 'stop'], env=env,
                       capture_output=True, timeout=30)
        shutil.rmtree(runtime, ignore_errors=True)
    print('LIVE_UPDATE_RESULT_COMPLETE: ' + result['status'], flush=True)


if __name__ == '__main__':
    main()
