#!/usr/bin/env python3
"""Exercise restart and failed-create recovery in a private native host."""
import importlib.util
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import time

sys.dont_write_bytecode = True


def main():
    out = Path(os.environ.get('RESTART_SPACES_OUT', 'build/restart-spaces')).resolve()
    out.mkdir(parents=True, exist_ok=False)
    binaries = {name: shutil.which(name) for name in ['pmux', 'pmuxd', 'prismattyc-host', 'Xvfb', 'xdotool', 'ffmpeg']}
    assert all(binaries.values()), binaries
    result = {'binaries': binaries, 'sha256': {name: hashlib.sha256(Path(binaries[name]).read_bytes()).hexdigest()
              for name in ['pmux', 'pmuxd', 'prismattyc-host']}, 'checks': []}
    # A short private socket path avoids sockaddr_un's path limit. Evidence
    # and persistent data stay in the requested output directory.
    with tempfile.TemporaryDirectory(prefix='pmux-restart-') as runtime:
        for key in list(os.environ):
            if key.startswith(('PMUX', 'PRISMATTYC_', 'XDG_')) or key == 'WAYLAND_DISPLAY':
                del os.environ[key]
        for kind in ['data', 'config', 'state']:
            directory = out / kind
            directory.mkdir(mode=0o700)
            os.environ[f'XDG_{kind.upper()}_HOME'] = str(directory)
        home = out / 'home'
        home.mkdir()
        os.environ.update(HOME=str(home), SHELL='/bin/bash', XDG_RUNTIME_DIR=runtime,
                          PMUX_SOCKET=str(Path(runtime) / 'pmux.sock'), HOST_UX_OUT=str(out),
                          HOST_UX_NO_WM='1', WINIT_UNIX_BACKEND='x11')
        spec = importlib.util.spec_from_file_location('host_ux', Path(__file__).with_name('host-ux-e2e.py'))
        ux = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(ux)
        display = subprocess.Popen([binaries['Xvfb'], '-displayfd', '1', '-screen', '0', '1920x1080x24', '-nolisten', 'tcp'],
                                   stdout=subprocess.PIPE, stderr=(out / 'xvfb.log').open('w'))
        host = None
        try:
            os.environ['DISPLAY'] = ':' + display.stdout.readline().decode().strip()
            config = ('font_px = 16.0\nspace_rail = "bottom"\nwindow_padding_px = 5\npane_gap_px = 5\n'
                      'window_opacity = 1.0\nbell_toaster_ms = 5000\n')
            spaces = out / 'data/prismattyc/spaces'
            cache = Path(runtime) / 'pmux.attach-tabs.json'

            def snapshot():
                with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
                    client.settimeout(5)
                    client.connect(os.environ['PMUX_SOCKET'])
                    client.sendall(b'{"type":"snapshot","version":1,"request_id":1}\n')
                    response = json.loads(client.makefile('r').readline())
                    assert response['status'] == 'ok', response
                    return response['response']['snapshot']['sessions']

            def session(name):
                return next((row for row in snapshot() if row['name'] == name), None)

            def identity(name):
                row = session(name)
                pane = row['windows'][0]['panes'][0]
                return row['id'], pane['id'], pane['child_pid'], row.get('space_id')

            def state_for(name):
                state = host.status()
                return state if state['space'] == name and not state['space_open_pending'] else None

            def type_name(name):
                host.key('ctrl+a')
                ux.run('xdotool', 'type', '--clearmodifiers', '--delay', '30', name)
                host.key('Return')

            def click_rail(name=None):
                time.sleep(.5)  # Let startup/refit ConfigureNotify reach X11.
                geometry = dict(line.split('=', 1) for line in ux.run('xdotool', 'getwindowgeometry', '--shell', host.wid).splitlines())
                chip = next(row for row in host.status()['space_chips'] if row['name'] == name)
                x = int(geometry['X']) + chip['x'] + chip['width'] // 3
                y = int(geometry['Y']) + chip['y'] + chip['height'] // 2
                ux.run('xdotool', 'windowfocus', '--sync', host.wid)
                ux.run('xdotool', 'mousemove', '--sync', str(x), str(y), 'click', '1')

            def begin_create(space, name):
                click_rail()
                ux.wait_for(lambda: 'save-space' in host.status()['last_raster']['guards'], 'new Space dialog')
                type_name(space)
                ux.wait_for(lambda: 'restore-prompt' in host.status()['last_raster']['guards'], 'session naming dialog')
                type_name(name)

            def marker(name, marker):
                color = tuple(hashlib.sha256(marker.encode()).digest()[:3])
                rgb = ';'.join(map(str, color))
                command = f"printf '\\033[2J\\033[H\\033[48;2;{rgb}m{marker}\\033[3;75HEDGE\\033[0m\\033[5;1H'"
                ux.run('xdotool', 'type', '--clearmodifiers', '--delay', '30', command)
                host.key('Return')
                ux.wait_for(lambda: marker in [line.strip() for line in ux.run('pmux', 'save-buffer', name, '-').splitlines()], 'shell executes the marker command')
                def colored(frame):
                    return sum(row[x:x + 3] == bytes(color) for row in frame.rows for x in range(0, len(row), frame.bpp))
                def painted():
                    pixels, _ = host.capture(marker)
                    frame = ux.PNG(pixels)
                    edge = sum(row[x:x + 3] == bytes(color) for row in frame.rows
                               for x in range(int(frame.width * .8) * frame.bpp, len(row), frame.bpp))
                    return colored(frame) > 200 and edge > 100
                ux.wait_for(painted, 'guest output reaches presented pixels')
                frame, _ = host.display(marker)
                assert colored(frame) > 200, 'guest output absent from actual X11 display'
                result.setdefault('sessions_after_marker', {})[marker] = session(name)

            def launch(name):
                return ux.Host(name, [], config)

            def restore():
                ux.wait_for(lambda: 'restore-prompt' in host.status()['last_raster']['guards'], 'startup restore choice')
                host.key('Return')
                ux.wait_for(lambda: state_for('A'), 'restored Space A')

            # A bare desktop launch has no daemon. Create must bring it up.
            host = launch('first-launch')
            begin_create('A', 'a-shell')
            ux.wait_for(lambda: state_for('A'), 'create from a bare launch', timeout=20)
            a = identity('a-shell')
            marker('a-shell', 'FIRST_READY')
            result['checks'].append('create_without_daemon')

            # A duplicate must explain the failure and keep the name editable.
            begin_create('B', 'a-shell')
            def rejected():
                report = host.status()['last_space_open']
                return report if report and report['name'] == 'B' and report['error'] else None
            rejection = ux.wait_for(rejected, 'duplicate-name diagnostic')
            assert 'already in use' in rejection['error'], rejection
            assert identity('a-shell') == a
            assert not (spaces / 'B.json').exists()
            ux.wait_for(lambda: 'restore-prompt' in host.status()['last_raster']['guards'], 'editable failed-create dialog is painted')
            host.capture('duplicate-name')
            type_name('b-shell')
            ux.wait_for(lambda: state_for('B'), 'retry corrected session name')
            b = identity('b-shell')
            result['initial_identities'] = {'A': a, 'B': b}
            assert a[0] != b[0] and a[1] != b[1] and a[2] != b[2] and a[3] != b[3]
            marker('b-shell', 'B_READY')
            assert 'B_READY' not in ux.run('pmux', 'save-buffer', 'a-shell', '-')
            result['checks'].append('duplicate_name_retry_and_isolation')

            click_rail('A')
            ux.wait_for(lambda: state_for('A'), 'switch back to A')
            assert identity('a-shell') == a
            host.stop()
            host = launch('warm-restart')
            restore()
            assert identity('a-shell') == a and identity('b-shell') == b
            marker('a-shell', 'WARM_READY')
            host.stop()
            host = None
            result['checks'].append('window_restart_preserves_pty_and_input')

            saved = cache.read_bytes()
            assert json.loads(saved)['session_names'], 'restart cache has no durable session names'
            # Reuse A's old ID for an unrelated live session before restore.
            ux.run('pmux', 'stop')
            ux.run('pmux', 'up')
            ux.run('pmux', 'new', 'foreign', '--no-attach')
            foreign = identity('foreign')
            assert foreign[0] == a[0], (foreign, a)
            cache.write_bytes(saved)
            host = launch('daemon-restart')
            restore()
            time.sleep(2)
            assert session('a-shell') is None and session('b-shell') is None, 'restore launched saved commands'
            assert host.status()['focused_session'] == 'a-shell', 'lost saved session identity'
            host.capture('stopped-session')
            host.key('Return')
            ux.wait_for(lambda: session('a-shell'), 'Enter recreates the saved session')
            marker('a-shell', 'COLD_READY')
            assert session('b-shell') is None, 'reopen launched another saved session'
            assert identity('foreign') == foreign
            assert 'COLD_READY' not in ux.run('pmux', 'save-buffer', 'foreign', '-')
            result['checks'].append('daemon_restart_recycled_id_and_single_session_reopen')

            begin_create('C', 'c-shell')
            ux.wait_for(lambda: state_for('C'), 'create after restart recovery')
            marker('c-shell', 'AFTER_RESTART_READY')
            host.stop()
            host = None
            result['checks'].append('create_after_restart')

            # Original 0.1.306 caches have IDs only. Recover from the Space
            # definition instead of connecting to the foreign reused ID.
            ux.run('pmux', 'stop')
            ux.run('pmux', 'up')
            ux.run('pmux', 'new', 'legacy-foreign', '--no-attach')
            legacy = json.loads(saved)
            legacy.pop('session_names', None)
            legacy.pop('space_id', None)
            cache.write_text(json.dumps(legacy))
            host = launch('legacy-restart')
            restore()
            time.sleep(2)
            assert host.status()['focused_session'] == 'a-shell'
            assert session('a-shell') is None
            host.capture('legacy-stopped')
            host.key('Return')
            ux.wait_for(lambda: session('a-shell'), 'legacy saved session recovery')
            marker('a-shell', 'LEGACY_READY')
            assert 'LEGACY_READY' not in ux.run('pmux', 'save-buffer', 'legacy-foreign', '-')
            result['checks'].append('legacy_numeric_cache_recovery')
            host.stop()
            host = None

            # A stopped daemon must not make the restore choice unusable.
            ux.run('pmux', 'stop')
            host = launch('restore-without-daemon')
            restore()
            assert host.status()['focused_session'] == 'a-shell'
            assert not Path(os.environ['PMUX_SOCKET']).exists(), 'restoring layout started a daemon or command'
            host.key('Return')
            ux.wait_for(lambda: session('a-shell'), 'Enter starts the daemon and restores one session')
            marker('a-shell', 'DAEMON_STARTED_READY')
            result['checks'].append('restore_without_daemon_then_reopen')
            host.stop()
            host = None

            # A removed Space must not resurrect cached sessions under its name.
            ux.run('pmux', 'space', 'rm', 'A')
            host = launch('deleted-space')
            ux.wait_for(lambda: 'restore-prompt' in host.status()['last_raster']['guards'], 'restore choice for deleted Space')
            host.key('Return')
            ux.wait_for(lambda: 'saved Space' in (host.directory / 'host.log').read_text(), 'deleted Space diagnostic')
            assert host.status()['space'] is None
            host.capture('deleted-space-error')
            begin_create('D', 'd-shell')
            ux.wait_for(lambda: state_for('D'), 'create after failed restore')
            marker('d-shell', 'RECOVERED_CREATE_READY')
            result['checks'].append('deleted_space_fails_without_blocking_create')
            result['status'] = 'PASS'
            print('RESTART_SPACES_E2E_COMPLETE: ' + ', '.join(result['checks']), flush=True)
        except Exception as error:
            result.update(status='FAIL', error=str(error))
            if host is not None:
                try:
                    result['last_window'] = host.status()
                    host.capture('failure')
                except (OSError, AssertionError, subprocess.CalledProcessError):
                    pass
            raise
        finally:
            if host is not None:
                host.stop()
            subprocess.run(['pmux', 'stop'], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            display.terminate()
            display.wait(timeout=5)
            (out / 'result.json').write_text(json.dumps(result, indent=2) + '\n')


if __name__ == '__main__':
    main()
