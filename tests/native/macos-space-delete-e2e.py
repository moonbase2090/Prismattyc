#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Exercise saved-space deletion through the native rail menu on both Mac presenters.

Requires a logged-in macOS desktop and Accessibility permission for the runner.
Every case owns a private daemon, home, runtime directory, and host process.
"""
import argparse
import ctypes as C
import json
import os
import socket
from pathlib import Path
import subprocess
import sys
import tempfile
import time


def stop(process):
    if process is not None and process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)


def run_case(bins, root, backend, case, allow_missing_panic_log):
    root.mkdir()
    runtime = root / 'runtime'
    runtime.mkdir(mode=0o700)
    for name in ('home', 'config', 'data', 'state'):
        (root / name).mkdir()
    config = root / 'config.toml'
    config.write_text(
        f'macos_present = "{backend}"\n'
        'space_rail = "left"\nspace_startup = "restore"\n'
        'splash = false\nstart_at_login = false\nautomatic_update_checks = false\n'
        'install_agent_skills = false\nrender_timer = "log"\n'
        'render_timer_log_every_frame = true\n[keys]\nspace_rail_focus = "ctrl+alt+f5"\n')
    env = {key: value for key, value in os.environ.items()
           if not key.startswith(('PMUX', 'PRISMATTYC_', 'XDG_'))}
    env.update(HOME=str(root / 'home'), SHELL='/bin/sh',
               PATH=f'{bins}:/usr/bin:/bin:/usr/sbin:/sbin',
               PMUX_SOCKET=str(runtime / 'pmux.sock'),
               XDG_RUNTIME_DIR=str(runtime), XDG_CONFIG_HOME=str(root / 'config'),
               XDG_DATA_HOME=str(root / 'data'), XDG_STATE_HOME=str(root / 'state'),
               PRISMATTYC_CONFIG=str(config), PRISMATTYC_NO_AGENT_SKILLS='1',
               PRISMATTYC_DUMP_PRESENT=str(root / 'present.png'))
    daemon = host = None
    result = {'backend': backend, 'case': case, 'status': 'FAIL'}

    def cli(*args):
        process = subprocess.run([str(bins / 'pmux'), *map(str, args)], env=env,
                                 capture_output=True, text=True, timeout=20)
        if process.returncode:
            raise subprocess.CalledProcessError(process.returncode, args, process.stdout, process.stderr)
        return process.stdout

    def wait(probe, label):
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            if host is not None:
                assert host.poll() is None, f'host exited during {label}: {host.returncode}'
            try:
                value = probe()
                if value:
                    return value
            except (FileNotFoundError, ValueError, KeyError, subprocess.CalledProcessError):
                pass
            time.sleep(.1)
        raise AssertionError('timeout: ' + label)

    def status():
        deadline = time.monotonic() + 10
        while True:
            assert host.poll() is None, f'host exited: {host.returncode}'
            try:
                data = json.loads(cli('render-status', '--json'))
                if data.get('host_pid') != host.pid or not data.get('windows'):
                    return {}
                return data['windows'][0]
            except (subprocess.CalledProcessError, ValueError):
                if time.monotonic() >= deadline:
                    raise
                time.sleep(.1)

    def sessions():
        with socket.socket(socket.AF_UNIX) as stream:
            stream.settimeout(5)
            stream.connect(env['PMUX_SOCKET'])
            stream.sendall(b'{"version":1,"request_id":1,"type":"snapshot"}\n')
            snapshot = json.loads(stream.makefile().readline())['response']['snapshot']
        return sorted((session['id'], session['name'], window['id'], pane['id'], pane['child_pid'])
                      for session in snapshot['sessions'] for window in session['windows']
                      for pane in window['panes'])

    def key(code, flags=0):
        for pressed in (True, False):
            event = quartz.CGEventCreateKeyboardEvent(None, code, pressed)
            assert event, 'keyboard event allocation'
            quartz.CGEventSetFlags(event, flags)
            quartz.CGEventPostToPid(host.pid, event)
            quartz.CFRelease(event)
        time.sleep(.12)

    def launch():
        with (root / 'host.log').open('a') as log:
            return subprocess.Popen([str(bins / 'prismattyc-host'), '--no-splash',
                                     '--attach-session', 'alpha-1'], env=env,
                                    stdout=log, stderr=log)

    try:
        with (root / 'daemon.log').open('w') as log:
            daemon = subprocess.Popen([str(bins / 'pmuxd'), '--socket', env['PMUX_SOCKET'],
                                       '--', '/bin/sh'], env=env, stdout=log, stderr=log)
        wait(lambda: Path(env['PMUX_SOCKET']).exists(), 'daemon socket')
        cli('space', 'create', 'alpha', '--no-attach')
        if case != 'last':
            cli('space', 'create', 'beta', '--no-attach')
        target = 'beta' if case == 'live-inactive' else 'alpha'
        before = sessions()
        pane = next(row[3] for row in before if row[1] == target + '-1')
        cli('send', pane, "printf 'DELETE_PROOF_ALIVE\\n'", '--enter')
        wait(lambda: 'DELETE_PROOF_ALIVE' in cli('save-buffer', target + '-1', '-'), 'live pane')
        host = launch()
        wait(lambda: status().get('space') == 'alpha', 'initial host space')
        wait(lambda: f'present_backend={backend}' in (root / 'host.log').read_text(), 'presenter')
        if case == 'after-restart':
            stop(host)
            host = launch()
            wait(lambda: status().get('space') == 'alpha', 'restarted host')
        names = status()['space_order']
        assert target in names
        if case == 'last':
            assert names == ['alpha'], names
        key(96, (1 << 18) | (1 << 19))  # Ctrl-Alt-F5, configured rail focus.
        key(115)  # Home.
        for _ in range(names.index(target)):
            key(125)  # Down.
        key(109, 1 << 17)  # Shift-F10: rail context menu.
        wait(lambda: status().get('context_menu'), 'rail context menu')
        assert status()['context_menu']['target'] == 'rail-space'
        key(119)  # End: Remove saved space.
        key(36)  # Enter arms confirmation.
        wait(lambda: status().get('context_menu', {}).get('confirmation') is not None,
             'delete confirmation')
        before_frame = status()['last_raster']['unix_ms']
        key(36)  # Enter confirms.
        wait(lambda: target not in status().get('space_order', names), 'saved space removed')
        wait(lambda: status().get('last_raster', {}).get('present_succeeded')
             and status()['last_raster']['unix_ms'] > before_frame, 'frame after deletion')
        time.sleep(1)
        after = sessions()
        assert after == before, 'deleting a saved layout changed live sessions'
        assert 'DELETE_PROOF_ALIVE' in cli('save-buffer', target + '-1', '-')
        assert host.poll() is None
        assert (root / 'present.png').stat().st_size > 0
        panic_log = runtime / 'host-panic.log'
        assert allow_missing_panic_log or panic_log.exists(), 'panic log was not installed'
        if panic_log.exists():
            assert panic_log.stat().st_mode & 0o777 == 0o600
            assert not panic_log.read_text(), 'unexpected panic; inspect host-panic.log'
        result.update(status='PASS', sessions='preserved', pane_output='preserved',
                      saved_spaces=status()['space_order'])
        print(f'PASS {backend} {case}', flush=True)
    except Exception as error:
        result['error'] = str(error)
        if isinstance(error, subprocess.CalledProcessError):
            result['stderr'] = error.stderr
        result['host_exit'] = host.poll() if host is not None else None
        raise
    finally:
        (root / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
        stop(host)
        stop(daemon)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bins', type=Path, required=True)
    parser.add_argument('--out', type=Path)
    parser.add_argument('--repeat', type=int, default=1)
    parser.add_argument('--allow-missing-panic-log', action='store_true',
                        help='allow older builds without the panic hook for reproduction')
    args = parser.parse_args()
    assert sys.platform == 'darwin', 'requires macOS'
    assert args.repeat > 0, '--repeat must be positive'
    assert quartz.AXIsProcessTrusted(), 'runner needs Accessibility permission'
    root = args.out or Path(tempfile.mkdtemp(prefix='space-delete-', dir='/tmp'))
    root = root.resolve()
    root.mkdir(parents=True, exist_ok=True)
    print(f'Evidence: {root}', flush=True)
    failures = []
    for iteration in range(args.repeat):
        for backend in ('tiles', 'iosurface'):
            for case in ('active', 'live-inactive', 'last', 'after-restart'):
                try:
                    run_case(args.bins.resolve(), root / f'{iteration}-{backend}-{case}',
                             backend, case, args.allow_missing_panic_log)
                except Exception as error:
                    failures.append(f'{iteration} {backend} {case}: {error}')
                    print('FAIL ' + failures[-1], flush=True)
    assert not failures, failures


if __name__ == '__main__':
    quartz = C.CDLL('/System/Library/Frameworks/ApplicationServices.framework/ApplicationServices')
    quartz.AXIsProcessTrusted.restype = C.c_bool
    quartz.CGEventCreateKeyboardEvent.argtypes = [C.c_void_p, C.c_uint16, C.c_bool]
    quartz.CGEventCreateKeyboardEvent.restype = C.c_void_p
    quartz.CGEventSetFlags.argtypes = [C.c_void_p, C.c_uint64]
    quartz.CGEventPostToPid.argtypes = [C.c_int, C.c_void_p]
    quartz.CFRelease.argtypes = [C.c_void_p]
    main()
