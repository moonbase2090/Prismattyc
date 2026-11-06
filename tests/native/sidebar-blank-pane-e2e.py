#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Record a blank terminal across native sidebar navigation on an isolated X11 display."""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bins', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--expect', choices=['missing', 'present'], required=True)
    args = parser.parse_args()
    bins, out = args.bins.resolve(), args.out.resolve()
    out.mkdir(parents=True, exist_ok=False)
    scratch = Path(tempfile.mkdtemp(prefix='sidebar-', dir='/tmp'))
    processes = []
    result = {'status': 'FAIL', 'expected_inactive_row': args.expect,
              'driver_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
              'binary_sha256': {name: hashlib.sha256((bins / name).read_bytes()).hexdigest()
                                for name in ['prismattyc-host', 'pmux', 'pmuxd']}}
    inherited = dict(os.environ)
    try:
        for key in list(os.environ):
            if key.startswith(('PMUX', 'PRISMATTYC_', 'XDG_')) or key == 'WAYLAND_DISPLAY':
                del os.environ[key]
        for name in ['home', 'data', 'state', 'config', 'run']:
            (scratch / name).mkdir(mode=0o700)
        shell = scratch / 'shell'
        shell.write_text("#!/bin/sh\nPS1='proof> '\nexport PS1\nexec /bin/sh\n")
        shell.chmod(0o700)
        os.environ.update(HOME=str(scratch / 'home'), SHELL=str(shell),
                          PATH=str(bins) + os.pathsep + inherited['PATH'],
                          PMUX_SOCKET=str(scratch / 'run/pmux.sock'),
                          XDG_RUNTIME_DIR=str(scratch / 'run'),
                          XDG_CONFIG_HOME=str(scratch / 'config'),
                          XDG_DATA_HOME=str(scratch / 'data'),
                          XDG_STATE_HOME=str(scratch / 'state'),
                          PRISMATTYC_DEFAULT_SPACE='0', PRISMATTYC_NO_AGENT_SKILLS='1',
                          HOST_UX_NO_WM='1', HOST_UX_OUT=str(out))

        def cli(*args):
            p = subprocess.run([str(bins / 'pmux'), *map(str, args)],
                               capture_output=True, text=True, timeout=20)
            with (out / 'commands.jsonl').open('a') as log:
                log.write(json.dumps({'args': args, 'exit': p.returncode,
                                      'stdout': p.stdout, 'stderr': p.stderr}) + '\n')
            p.check_returncode()
            return p.stdout

        def wait(probe, label):
            deadline = time.monotonic() + 25
            while time.monotonic() < deadline:
                assert all(p.poll() is None for p in processes), 'owned process exited'
                try:
                    value = probe()
                    if value:
                        return value
                except (OSError, ValueError, KeyError, subprocess.CalledProcessError):
                    pass
                time.sleep(.1)
            raise AssertionError('timeout: ' + label)

        def identities():
            sys.path.insert(0, str(Path(__file__).resolve().parents[2] / 'scripts'))
            from native_control import control_socket
            with control_socket(os.environ['PMUX_SOCKET']) as stream:
                stream.sendall(b'{"type":"snapshot","version":1,"request_id":1}\n')
                with stream.makefile('rb') as reader:
                    snapshot = json.loads(reader.readline())['response']['snapshot']
            return sorted((s['id'], s['name'], w['id'], p['id'], p['child_pid'])
                          for s in snapshot['sessions'] for w in s['windows'] for p in w['panes'])

        log = (out / 'daemon.log').open('w')
        daemon = subprocess.Popen([str(bins / 'pmuxd'), '--socket', os.environ['PMUX_SOCKET'],
                                   '--', str(shell)], stdout=log, stderr=log)
        processes.append(daemon)
        wait(lambda: Path(os.environ['PMUX_SOCKET']).is_socket(), 'private daemon')
        result['version'] = cli('--version').strip()
        for name in ['alpha', 'default', 'beta']:
            cli('space', 'create', name, '--session-name', name + '-seat', '--no-attach')
        spec = importlib.util.spec_from_file_location('ux', Path(__file__).with_name('host-ux-e2e.py'))
        ux = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(ux)
        display = subprocess.Popen(['Xvfb', '-displayfd', '1', '-screen', '0', '1400x900x24',
                                    '-nolisten', 'tcp', '-noreset'], stdout=subprocess.PIPE,
                                   stderr=(out / 'xvfb.log').open('w'))
        processes.append(display)
        os.environ['DISPLAY'] = ':' + display.stdout.readline().decode().strip()
        host = ux.Host('native', ['--attach-session', 'default-seat'],
                       'splash=false\nlayout="sidebar"\nchrome_style="graphite"\n'
                       'start_at_login=false\nautomatic_update_checks=false\n'
                       'install_agent_skills=false\nspace_startup="restore"\n')
        processes.append(host.process)
        wait(lambda: host.status().get('space') == 'default', 'initial default Space')
        assert 'ignoring config' not in (host.directory / 'host.log').read_text()
        wait(lambda: len(host.status()['sidebar_space_rows']) == 3, 'painted sidebar Spaces')
        row = next(r for r in host.status()['sidebar_space_rows'] if r['name'] == 'alpha')
        ux.run('xdotool', 'mousemove', '--window', host.wid,
               str(row['x'] + 100), str(row['y'] + row['height'] * 3 // 2))
        ux.run('xdotool', 'click', '1')
        wait(lambda: host.status()['space'] == 'alpha' and not host.status()['space_open_pending'],
             'initial sidebar navigation to Space A')
        cli('ls')
        sessions_before = identities()
        time.sleep(.7)
        host.key('ctrl+alt+shift+e')
        wait(lambda: len(host.status()['local_terminal_panes']) == 1, 'blank split')
        pane_id = host.status()['focused_pane']

        def blank(state):
            return next(p for p in state['current_panes'] if p['pane_id'] == f'PaneId({pane_id})')

        child = blank(host.status())['local_child_pid']
        token = scratch / 'token'
        command = "export SIDEBAR_KEEP=alive; printf 'I am a blank terminal\\n'"
        ux.run('xdotool', 'type', '--clearmodifiers', '--delay', '1', command)
        host.key('Return')
        time.sleep(.7)

        def capture(label):
            time.sleep(.5)
            state = host.status()
            assert state['last_raster']['present_succeeded']
            (out / (label + '.json')).write_text(json.dumps(state, indent=2) + '\n')
            host.capture(label)
            return state

        before = capture('active-alpha')

        def space_y(state, name):
            return next(r['y'] for r in state['sidebar_space_rows'] if r['name'] == name)

        anchor = space_y(before, 'beta')
        original_rows = [(r['kind'], r['label']) for r in before.get('sidebar_rows', [])
                         if r['space'] == 'alpha']
        observations = []
        for target in ['default', 'beta', 'alpha']:
            row = next(r for r in host.status()['sidebar_space_rows'] if r['name'] == target)
            ux.run('xdotool', 'mousemove', '--window', host.wid,
                   str(row['x'] + 100), str(row['y'] + row['height'] * 3 // 2))
            ux.run('xdotool', 'click', '1')
            wait(lambda: host.status()['space'] == target and not host.status()['space_open_pending'],
                 'native sidebar navigation to ' + target)
            state = capture('visit-' + target)
            os.kill(child, 0)
            observations.append({'space': target, 'original_child_alive': True,
                                 'beta_row_y': space_y(state, 'beta'),
                                 'alpha_rows': [(r['kind'], r['label'])
                                                for r in state.get('sidebar_rows', [])
                                                if r['space'] == 'alpha']})
        returned = host.status()
        assert pane_id in returned['local_terminal_panes']
        assert blank(returned)['local_child_pid'] == child
        _, slot = next(item for item in returned['visible_pane_slots'] if item[0] == pane_id)
        ux.run('xdotool', 'mousemove', '--window', host.wid,
               str(slot[0] + 40), str(slot[1] + 90))
        ux.run('xdotool', 'click', '1')
        wait(lambda: host.status()['focused_pane'] == pane_id, 'focus returned blank terminal')
        ux.run('xdotool', 'type', '--clearmodifiers', '--delay', '1',
               'printf %s "$SIDEBAR_KEEP" > ' + str(token))
        host.key('Return')
        wait(lambda: token.exists(), 'retained shell state')
        assert token.read_text() == 'alive'
        assert identities() == sessions_before, 'navigation changed daemon session/process identities'
        missing = [item['beta_row_y'] < anchor for item in observations[:2]]
        result.update(original_child_preserved=True, shell_state='alive',
                      observations=observations, original_beta_row_y=anchor,
                      inactive_blank_row_missing=missing)
        assert missing == [args.expect == 'missing'] * 2, (anchor, observations)
        assert observations[-1]['beta_row_y'] == anchor
        if original_rows and args.expect == 'present':
            assert all(item['alpha_rows'] == original_rows for item in observations)
        result['status'] = 'PASS'
    except BaseException as error:
        result['error'] = str(error)
        if 'host' in locals() and host.process.poll() is None:
            try:
                (out / 'failure-status.json').write_text(json.dumps(host.status(), indent=2) + '\n')
                host.capture('failure')
            except Exception as diagnostic:
                result['diagnostic_error'] = str(diagnostic)
        raise
    finally:
        for process in reversed(processes):
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)
        result['owned_processes_reaped'] = all(p.poll() is not None for p in processes)
        shutil.rmtree(scratch)
        result['scratch_removed'] = not scratch.exists()
        (out / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
        os.environ.clear()
        os.environ.update(inherited)
    print(json.dumps(result))


if __name__ == '__main__':
    main()
