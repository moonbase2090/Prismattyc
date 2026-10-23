#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Prove session restore and mail durability; optionally drive the macOS update UI.

--upgrade-app is destructive only to the specified app inside a disposable VM.
It runs the real release updater against that installed app. Never use it on
an owner's installation. --native exercises the current-release restart path
through native keys, with a private daemon and configuration.
"""
import argparse
import ctypes as C
import json
import os
from pathlib import Path
import shlex
import shutil
import socket
import subprocess
import sys
import tempfile
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bins', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--upgrade-app', type=Path)
    parser.add_argument('--native', action='store_true')
    args = parser.parse_args()
    assert not (args.native and args.upgrade_app), 'Record installer and native UI drives separately'
    if args.native:
        assert sys.platform == 'darwin', 'Native keys require a macOS desktop'
    bins, out = args.bins.resolve(), args.out.resolve()
    out.mkdir(parents=True, exist_ok=False)
    runtime = Path(tempfile.mkdtemp(prefix='pupdate-', dir='/tmp'))
    for name in ('home', 'config', 'data', 'state', 'project'):
        (out / name).mkdir()
    config = out / 'config.toml'
    config.write_text('splash = false\nstart_at_login = false\nautomatic_update_checks = false\n'
                      'install_agent_skills = false\nspace_startup = "restore"\n'
                      'chrome_style = "graphite"\n[keys]\nupdate_restart = "ctrl+alt+u"\n')
    env = {k: v for k, v in os.environ.items() if not k.startswith(('PMUX', 'PRISMATTYC_', 'XDG_'))}
    env.update(HOME=str(out / 'home'), SHELL='/bin/sh', PS1='verify> ',
               PATH=f'{bins}:/usr/bin:/bin:/usr/sbin:/sbin', PMUX=str(bins / 'pmux'),
               PMUX_SOCKET=str(runtime / 'pmux.sock'), XDG_RUNTIME_DIR=str(runtime),
               XDG_DATA_HOME=str(out / 'data'), XDG_CONFIG_HOME=str(out / 'config'),
               XDG_STATE_HOME=str(out / 'state'), PRISMATTYC_CONFIG=str(config),
               PRISMATTYC_NO_AGENT_SKILLS='1', PRISMATTYC_DUMP_PRESENT=str(out / 'present.png'))
    daemon = host = None
    result = {'status': 'FAIL', 'mode': 'native' if args.native else 'cli', 'checks': []}

    def command(executable, *words, timeout=90):
        p = subprocess.run([str(executable), *map(str, words)], env=env, cwd=out / 'project',
                           capture_output=True, text=True, timeout=timeout)
        with (out / 'commands.jsonl').open('a') as log:
            log.write(json.dumps({'executable': str(executable), 'args': list(map(str, words)),
                                  'exit': p.returncode, 'stdout': p.stdout, 'stderr': p.stderr}) + '\n')
        if p.returncode:
            raise subprocess.CalledProcessError(p.returncode, words, p.stdout, p.stderr)
        return p.stdout

    def cli(*words):
        return command(bins / 'pmux', '--socket', env['PMUX_SOCKET'], *words)

    def snapshot():
        with socket.socket(socket.AF_UNIX) as stream:
            stream.settimeout(5)
            stream.connect(env['PMUX_SOCKET'])
            stream.sendall(b'{"version":1,"request_id":1,"type":"snapshot"}\n')
            return json.loads(stream.makefile().readline())['response']['snapshot']

    def wait(probe, label, timeout=30):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            try:
                value = probe()
                if value:
                    return value
            except (OSError, KeyError, ValueError, subprocess.CalledProcessError):
                pass
            time.sleep(.1)
        raise AssertionError('timeout: ' + label)

    def record(check):
        result['checks'].append(check)
        print('PASS ' + check, flush=True)

    def render_status():
        data = json.loads(cli('render-status', '--json'))
        assert data['host_pid'] == host.pid, 'unexpected host identity'
        return data

    def status():
        return render_status()['windows'][0]

    def capture(name):
        initial = render_status()
        identity = (initial['host_pid'], initial['host_instance'])
        previous = initial['windows'][0]['last_raster']['capture_seq']
        key(119)  # End changes only the selected row and requests a new frame.

        def fresh_capture():
            data = render_status()
            if (data['host_pid'], data['host_instance']) != identity:
                return None
            window = data['windows'][0]
            frame = window.get('last_raster', {})
            if not frame.get('present_succeeded') or frame.get('capture_seq', 0) <= previous:
                return None
            before = (out / 'present.json').read_bytes()
            metadata = json.loads(before)
            if (metadata['host_pid'], metadata['host_instance'], metadata['seq']) != (*identity, frame['capture_seq']):
                return None
            pixels = (out / 'present.png').read_bytes()
            if before != (out / 'present.json').read_bytes():
                return None
            return window, pixels, metadata

        window, pixels, metadata = wait(fresh_capture, 'fresh frame from the expected host instance')
        (out / f'{name}.json').write_text(json.dumps(window, indent=2))
        (out / f'{name}-frame.json').write_text(json.dumps(metadata, indent=2))
        (out / f'{name}.png').write_bytes(pixels)

    def key(code, flags=0):
        for pressed in (True, False):
            event = quartz.CGEventCreateKeyboardEvent(None, code, pressed)
            assert event
            quartz.CGEventSetFlags(event, flags)
            quartz.CGEventPostToPid(host.pid, event)
            quartz.CFRelease(event)
        time.sleep(.15)

    def choose(label):
        def ready_rows():
            panel = status().get('space_details')
            rows = panel['rows'] if panel else []
            return rows if any(text.startswith(label + ':') for text in rows) else None
        rows = wait(ready_rows, 'action ' + label)
        index = next(i for i, text in enumerate(rows) if text.startswith(label + ':'))
        key(115)  # Home.
        for _ in range(index):
            key(125)  # Down.
        key(36)  # Enter.

    try:
        initial_bins = args.upgrade_app / 'Contents/MacOS' if args.upgrade_app else bins
        with (out / 'daemon.log').open('w') as log:
            daemon = subprocess.Popen([str(initial_bins / 'pmuxd'), '--socket', env['PMUX_SOCKET'],
                                       '--', '/bin/sh'], env=env, cwd=out / 'project', stdout=log, stderr=log)
        wait(lambda: Path(env['PMUX_SOCKET']).is_socket(), 'private daemon')
        cli('doctor')
        cli('new', '--no-attach', 'work', '--', '/bin/sh')
        session = next(s for s in snapshot()['sessions'] if s['name'] == 'work')
        pane = session['windows'][0]['panes'][0]
        launch = [sys.executable, '-c', "from pathlib import Path; import time; f=Path('starts').open('a'); f.write('x'); f.close(); print('CONFIG_COMMAND_OK', flush=True); time.sleep(600)"]
        cli('pane-write', str(pane['id']), '--text', shlex.join(launch), '--submit', 'enter', '--json')
        marker = out / 'project/starts'
        wait(lambda: marker.exists() and marker.read_text() == 'x', 'initial command')
        cli('mail', '--as', 'work', 'send', 'work', '--summary', 'restore verification', '--body', 'durable-mail-proof')
        old_pid = json.loads(cli('versions'))['daemon']['pid']
        (out / 'before.json').write_text(json.dumps(snapshot(), indent=2))
        if args.native:
            host = subprocess.Popen([str(bins / 'prismattyc-host'), '--no-splash', '--attach-session', 'work'],
                                    env=env, stdout=(out / 'host.log').open('w'), stderr=subprocess.STDOUT)
            wait(lambda: status().get('last_raster', {}).get('present_succeeded'), 'native host frame')
            initial_instance = render_status()['host_instance']
            key(32, (1 << 18) | (1 << 19))  # Ctrl-Alt-U.
            wait(lambda: status().get('space_details'), 'maintenance menu')
            choose('Update and restart all components')
            wait(lambda: any('Save and continue:' in r for r in status()['space_details']['rows']), 'session consent')
            capture('session-consent')
            choose('Cancel')
            assert json.loads(cli('versions'))['daemon']['pid'] == old_pid
            assert marker.read_text() == 'x'
            record('cancel preserves daemon and command')
            key(53)  # Escape.
            key(32, (1 << 18) | (1 << 19))
            choose('Update and restart all components')
            wait(lambda: any('Save and continue:' in r for r in status()['space_details']['rows']), 'second consent')
            choose('Save and continue')
            wait(lambda: render_status()['host_instance'] != initial_instance and any('Restore sessions:' in r for r in (status().get('space_details') or {}).get('rows', [])), 'replacement host restore offer', timeout=90)
            (out / 'before-restore.json').write_text(json.dumps(snapshot(), indent=2))
            assert not any(s['name'] == 'work' for s in snapshot()['sessions']), 'startup recreated a session before restore consent'
            capture('restore-offer')
            choose('Restore sessions')
            wait(lambda: marker.read_text() == 'xx', 'saved command relaunched')
            wait(lambda: any('Session restoration finished.' in r for r in (status().get('space_details') or {}).get('rows', [])), 'restoration result')
            wait(lambda: status().get('focused_session') == 'work', 'restored window session')
            capture('restore-result')
        else:
            backup = out / 'configuration.json'
            cli('session-config', 'save', backup)
            saved = json.loads(backup.read_text())
            work = next(s for s in saved['space']['sessions'] if s['name'] == 'work')
            assert work['windows'][0]['root']['cwd'] == str((out / 'project').resolve())
            assert work['windows'][0]['root']['command'], 'foreground command was not saved'
            if args.upgrade_app:
                before = command(args.upgrade_app / 'Contents/MacOS/pmux', '--version')
                assert '0.3.38' in before, before
                report = json.loads(command(bins / 'pmux', 'update', '--json', timeout=300))
                assert report['status'] == 'installed', report
                after = command(args.upgrade_app / 'Contents/MacOS/pmux', '--version')
                assert report['version'] in after and '0.3.38' not in after, after
                result['release_update'] = {'before': before.strip(), 'after': after.strip(), 'report': report}
                env['PMUX_SERVER'] = str(args.upgrade_app / 'Contents/MacOS/pmuxd')
                record('real signed release upgrade from 0.3.38')
            else:
                env['PMUX_SERVER'] = str(bins / 'pmuxd')
            cli('restart', '--daemon', '--stop-sessions', '--worker', '--json')
            cli('session-config', 'restore', backup)
            wait(lambda: marker.read_text() == 'xx', 'saved command relaunched')
            repeated = cli('session-config', 'restore', backup)
            assert 'Restored 0 sessions.' in repeated, repeated
        assert json.loads(cli('versions'))['daemon']['pid'] != old_pid
        assert marker.read_text() == 'xx', 'a command was lost or replayed'
        letters = json.loads(cli('mail', '--as', 'work', 'claim', '--json'))['letters']
        assert len(letters) == 1 and letters[0]['body'] == 'durable-mail-proof'
        cli('mail', '--as', 'work', 'commit', letters[0]['id'])
        cli('doctor')
        (out / 'after.json').write_text(json.dumps(snapshot(), indent=2))
        (out / 'pane.txt').write_text(cli('save-buffer', 'work', '-'))
        record('daemon replaced and saved command restored exactly once')
        record('mail survives daemon restart and session recreation')
        result['status'] = 'PASS'
    except Exception as error:
        result['error'] = repr(error)
        raise
    finally:
        if host is not None and host.poll() is None:
            host.terminate()
            host.wait(timeout=10)
        if Path(env['PMUX_SOCKET']).exists():
            try:
                cli('stop')
            except Exception as error:
                result['cleanup_error'] = repr(error)
        if daemon is not None:
            try:
                daemon.wait(timeout=10)
            except subprocess.TimeoutExpired:
                daemon.terminate()
                daemon.wait(timeout=10)
        result['socket_removed'] = not Path(env['PMUX_SOCKET']).exists()
        shutil.rmtree(runtime)
        (out / 'result.json').write_text(json.dumps(result, indent=2) + '\n')


if __name__ == '__main__':
    if '--native' in sys.argv:
        quartz = C.CDLL('/System/Library/Frameworks/ApplicationServices.framework/ApplicationServices')
        quartz.AXIsProcessTrusted.restype = C.c_bool
        assert quartz.AXIsProcessTrusted(), 'Accessibility permission is required for native keys'
        quartz.CGEventCreateKeyboardEvent.argtypes = [C.c_void_p, C.c_uint16, C.c_bool]
        quartz.CGEventCreateKeyboardEvent.restype = C.c_void_p
        quartz.CGEventSetFlags.argtypes = [C.c_void_p, C.c_uint64]
        quartz.CGEventPostToPid.argtypes = [C.c_int, C.c_void_p]
        quartz.CFRelease.argtypes = [C.c_void_p]
    main()
