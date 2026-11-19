#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Open the Update and restart modal in a real window and capture its states.

The run starts a private daemon with --sessions sessions, opens Update and
restart from its menu, and keeps the presented frame (PRISMATTYC_DUMP_PRESENT)
for the collapsed modal, the session list shown with the keyboard, and the
list shown with the mouse. It then cancels and checks every session still
runs. Nothing is installed or restarted.

The modal reads "Restart to update OLD -> NEW" only when the binaries are
older than the newest release; otherwise it reads "Restart components".

Keys and clicks go to the host window only: XTEST through xdotool on Linux,
SendInput on Windows. --key-relay DIR (any platform, used for a macOS guest
whose privacy settings forbid event injection) writes each input as
DIR/NNNNN.req (JSON) and waits for DIR/NNNNN.ack from an outside source.

  restart-modal-e2e.py --bins target/release --out DIR [--sessions 30]
"""
import argparse
import json
import os
import queue
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

EXE = '.exe' if os.name == 'nt' else ''
NAMES = ['api', 'web', 'docs', 'build', 'release', 'review', 'notes', 'scratch', 'deploy',
         'search', 'design', 'tests', 'bench', 'infra', 'mobile', 'cli', 'fonts', 'theme']


class Input:
    """Native keys and clicks for one host window."""

    VK = {'y': 0x59, 'Down': 0x28, 'Up': 0x26, 'Home': 0x24, 'Return': 0x0D, 'Escape': 0x1B}

    def __init__(self, pid, relay=None):
        self.pid = pid
        self.relay = relay
        self.sent = 0
        if relay:
            relay.mkdir(parents=True, exist_ok=True)

    def _relay(self, request):
        stem = self.relay / f'{self.sent:05d}'
        self.sent += 1
        stem.with_suffix('.req').write_text(json.dumps(request))
        deadline = time.monotonic() + 60
        while not stem.with_suffix('.ack').exists():
            assert time.monotonic() < deadline, f'no relay ack for {request}'
            time.sleep(.05)

    def _window(self):
        if sys.platform.startswith('linux'):
            return subprocess.check_output(['xdotool', 'search', '--sync', '--onlyvisible', '--pid',
                                            str(self.pid)], text=True).split()[0]
        import ctypes
        from ctypes import wintypes
        user32 = ctypes.WinDLL('user32', use_last_error=True)
        found = []

        @ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)
        def visit(hwnd, _):
            owner = wintypes.DWORD()
            user32.GetWindowThreadProcessId(hwnd, ctypes.byref(owner))
            if owner.value == self.pid and user32.IsWindowVisible(hwnd):
                found.append(hwnd)
            return True

        user32.EnumWindows(visit, 0)
        assert found, 'host window not found'
        user32.SetForegroundWindow(found[0])
        return found[0]

    def key(self, name, ctrl=False, shift=False):
        if self.relay:
            self._relay({'key': name, 'ctrl': ctrl, 'shift': shift})
        elif sys.platform.startswith('linux'):
            subprocess.check_call(['xdotool', 'windowfocus', '--sync', self._window()])
            chord = '+'.join([*(['ctrl'] if ctrl else []), *(['shift'] if shift else []), name])
            subprocess.check_call(['xdotool', 'key', '--clearmodifiers', chord])
        else:
            self._windows_input([*([0x11] if ctrl else []), *([0x10] if shift else []), self.VK[name]])
        time.sleep(.15)

    def click(self, x, y):
        """Left click at window pixel (x, y)."""
        if self.relay:
            self._relay({'click': [x, y]})
        elif sys.platform.startswith('linux'):
            window = self._window()
            subprocess.check_call(['xdotool', 'mousemove', '--window', window, str(x), str(y), 'click', '1'])
        else:
            import ctypes
            from ctypes import wintypes
            ctypes.WinDLL('shcore').SetProcessDpiAwareness(2)
            hwnd = self._window()
            point = wintypes.POINT(x, y)
            user32 = ctypes.WinDLL('user32', use_last_error=True)
            user32.ClientToScreen(hwnd, ctypes.byref(point))
            user32.SetCursorPos(point.x, point.y)
            time.sleep(.1)
            user32.mouse_event(0x0002, 0, 0, 0, 0)
            user32.mouse_event(0x0004, 0, 0, 0, 0)
        time.sleep(.2)

    def _windows_input(self, keys):
        import ctypes
        from ctypes import wintypes
        user32 = ctypes.WinDLL('user32', use_last_error=True)
        self._window()

        class KEYBDINPUT(ctypes.Structure):
            _fields_ = [('wVk', wintypes.WORD), ('wScan', wintypes.WORD), ('dwFlags', wintypes.DWORD),
                        ('time', wintypes.DWORD), ('dwExtraInfo', ctypes.c_size_t)]

        class INPUT(ctypes.Structure):
            class _U(ctypes.Union):
                _fields_ = [('ki', KEYBDINPUT), ('pad', ctypes.c_byte * 32)]
            _anonymous_ = ('u',)
            _fields_ = [('type', wintypes.DWORD), ('u', _U)]

        events = [(k, 0) for k in keys] + [(k, 2) for k in reversed(keys)]
        array = (INPUT * len(events))()
        for slot, (key, flags) in zip(array, events):
            slot.type = 1
            slot.ki = KEYBDINPUT(key, 0, flags, 0, 0)
        assert user32.SendInput(len(events), array, ctypes.sizeof(INPUT)) == len(events)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--bins', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--sessions', type=int, default=30)
    parser.add_argument('--key-relay', type=Path)
    parser.add_argument('--keyboard-only', action='store_true',
                        help='skip the mouse checks (an input relay that has no pointer mapping)')
    parser.add_argument('--restart-without-saving', action='store_true',
                        help='after the checks, choose Restart without saving and wait for the app to relaunch '
                             '(disposable machines only: it updates or restarts every component)')
    parser.add_argument('--timeout', type=int, default=600)
    args = parser.parse_args()
    bins, out = args.bins.resolve(), args.out.resolve()
    out.mkdir(parents=True, exist_ok=False)
    if args.restart_without_saving and sys.platform != 'darwin':
        # The updater refuses to replace a build tree; run from an installed copy.
        install = out / 'install' / 'bin'
        install.mkdir(parents=True)
        for name in ('pmux', 'pmuxd', 'pmux-attach', 'pmux-mcp', 'prismattyc', 'prismattyc-host'):
            shutil.copy2(bins / (name + EXE), install / (name + EXE))
        bins = install
    for name in ('home', 'config', 'data', 'state', 'project'):
        (out / name).mkdir()
    runtime = Path(tempfile.mkdtemp(prefix='pmodal-'))
    config = out / 'config.toml'
    config.write_text('splash = false\nstart_at_login = false\nautomatic_update_checks = false\n'
                      'install_agent_skills = false\nchrome_style = "graphite"\n'
                      # No Option/Alt: on macOS, Option+letter can be a dead key.
                      '[keys]\nupdate_restart = "ctrl+shift+y"\n')
    env = {k: v for k, v in os.environ.items() if not k.startswith(('PMUX', 'PRISMATTYC_', 'XDG_'))}
    socket_path = None if os.name == 'nt' else runtime / 'pmux.sock'
    env.update(PMUX=str(bins / ('pmux' + EXE)), PRISMATTYC_CONFIG=str(config),
               PRISMATTYC_NO_AGENT_SKILLS='1', PRISMATTYC_DUMP_PRESENT=str(out / 'present.png'))
    if socket_path:
        env['PMUX_SOCKET'] = str(socket_path)
    if os.name == 'nt':
        env.update(USERPROFILE=str(out / 'home'), LOCALAPPDATA=str(out / 'data'), APPDATA=str(out / 'config'))
    else:
        env.update(HOME=str(out / 'home'), SHELL='/bin/sh', XDG_RUNTIME_DIR=str(runtime),
                   XDG_DATA_HOME=str(out / 'data'), XDG_CONFIG_HOME=str(out / 'config'),
                   XDG_STATE_HOME=str(out / 'state'))
    result = {'status': 'FAIL', 'platform': sys.platform, 'checks': []}
    owned = []

    def cli(*words, timeout=60):
        socket = ['--socket', str(socket_path)] if socket_path else []
        p = subprocess.run([str(bins / ('pmux' + EXE)), *socket, *words], env=env, cwd=out / 'project',
                           capture_output=True, text=True, timeout=timeout)
        if p.returncode:
            raise AssertionError(f'pmux {" ".join(words)} exited {p.returncode}: {p.stdout}{p.stderr}')
        return p.stdout

    def wait(probe, label, timeout=60):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            try:
                value = probe()
                if value:
                    return value
            except (OSError, KeyError, IndexError, ValueError, TypeError, AssertionError):
                pass
            time.sleep(.1)
        raise AssertionError('timeout: ' + label)

    def window():
        return json.loads(cli('render-status', '--json'))['windows'][0]

    def modal():
        return window().get('restart_modal')

    def record(check):
        result['checks'].append(check)
        print('PASS ' + check, flush=True)

    def capture(name, ready):
        """Keep the first frame presented after `ready` holds."""
        state = wait(ready, name)
        # The frame for this state is presented within a few ticks; keep a
        # sidecar-consistent copy after that.
        time.sleep(.6)
        before = (out / 'present.json').read_bytes()
        pixels = (out / 'present.png').read_bytes()
        assert (out / 'present.json').read_bytes() == before, 'frame changed while copying'
        (out / f'{name}.png').write_bytes(pixels)
        (out / f'{name}.json').write_text(json.dumps({'modal': state, 'frame': json.loads(before)}, indent=2))
        return state

    def choose(keys, label):
        """Move the menu selection onto the row named `label`, then Enter.

        The row index depends on how the panel wraps details at this window
        width, so follow the reported selection instead of counting rows."""
        def panel():
            details = window().get('space_details') or {}
            rows = details.get('rows', [])
            return details if any(x.startswith(label + ':') for x in rows) else None

        def selected_row():
            details = wait(panel, 'row ' + label)
            return details['selected'], details

        keys.key('Home')
        time.sleep(.3)
        for step in ('Down', 'Up'):
            for _ in range(40):
                selected, details = selected_row()
                if selected is not None and details['rows'][selected].startswith(label + ':'):
                    keys.key('Return')
                    return
                keys.key(step)
                try:
                    wait(lambda: selected_row()[0] != selected, 'selection moves', timeout=3)
                except AssertionError:
                    break  # This end of the menu; try the other direction.
        raise AssertionError(f'could not select {label}: {details}')

    try:
        daemon_cmd = [str(bins / ('pmuxd' + EXE)), *(['--socket', str(socket_path)] if socket_path else [])]
        with (out / 'daemon.log').open('w') as log:
            owned.append(subprocess.Popen(daemon_cmd, env=env, cwd=out / 'project', stdout=log, stderr=log))
        wait(lambda: json.loads(cli('versions'))['daemon'].get('pid'), 'private daemon')
        names = [NAMES[i % len(NAMES)] + ('' if i < len(NAMES) else f'-{i // len(NAMES) + 1}')
                 for i in range(args.sessions)]
        for name in names:
            cli('new', '--no-attach', name)
        host = subprocess.Popen([str(bins / ('prismattyc-host' + EXE)), '--no-splash', '--attach-session', names[0]],
                                env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
                                errors='replace')
        owned.append(host)
        # Every app start prints its font line. A relaunched app keeps this
        # output (exec on Unix, an inheriting child on Windows), so the line
        # arriving again is the relaunch signal; the reader also keeps the log.
        starts = queue.Queue()

        def read_host_output():
            with (out / 'host.log').open('w') as log:
                for line in host.stdout:
                    log.write(line)
                    log.flush()
                    if line.startswith('prismattyc-host: font '):
                        starts.put(line)

        threading.Thread(target=read_host_output, daemon=True).start()
        wait(lambda: window()['last_raster']['present_succeeded'], 'first frame')
        # The modal lists what a restart would stop, including any session
        # the daemon or the window started on its own.
        planned = [name for _, name in json.loads(cli('restart', '--plan', '--json'))['sessions']]
        total = len(planned)
        result['sessions'] = total
        assert set(names) <= set(planned), planned
        keys = Input(host.pid, args.key_relay)
        keys.key('y', ctrl=True, shift=True)
        choose(keys, 'Update and restart all components')
        first = capture('collapsed', lambda: (lambda m: m if m and m['toggle_rect'] and not m['expanded'] else None)(modal()))
        result['collapsed'] = first
        assert first['sessions_shown'] == 0, first
        assert first['rows'][3].startswith(f'Show: {total} sessions'), first
        record(f'modal opens collapsed: {first["title"]} {first["version"]}, {total} sessions hidden')
        for _ in range(3):
            keys.key('Down')
        keys.key('Return')
        shown = capture('expanded-keyboard', lambda: (lambda m: m if m and m['expanded'] else None)(modal()))
        assert shown['sessions_shown'] == total and shown['rows'][3].startswith('Hide:'), shown
        record('keyboard Show lists every session')
        keys.key('Return')
        wait(lambda: modal() and not modal()['expanded'], 'keyboard Hide')
        record('keyboard Hide folds the list')
        if not args.keyboard_only:
            x, y, w, h = modal()['toggle_rect']
            keys.click(x + w // 2, y + h // 2)
            shown = capture('expanded-mouse', lambda: (lambda m: m if m and m['expanded'] else None)(modal()))
            record('mouse Show lists every session')
            x, y, w, h = shown['toggle_rect']
            keys.click(x + w // 2, y + h // 2)
            wait(lambda: modal() and not modal()['expanded'], 'mouse Hide')
            record('mouse Hide folds the list')
        keys.key('Escape')
        wait(lambda: window().get('restart_modal') is None, 'modal closed by Escape')
        after = [name for _, name in json.loads(cli('restart', '--plan', '--json'))['sessions']]
        result['sessions_after_cancel'] = len(after)
        assert sorted(after) == sorted(planned), after
        record('Escape cancels and every session keeps running')
        if args.restart_without_saving:
            first = json.loads(cli('render-status', '--json'))['host_instance']
            # Cancel left the result in the update panel; close it first.
            if window().get('space_details'):
                keys.key('Escape')
                wait(lambda: not window().get('space_details'), 'update panel closed')
            starts.get(timeout=60)  # the first start's line, read before the restart
            keys.key('y', ctrl=True, shift=True)
            choose(keys, 'Update and restart all components')
            dialog = wait(modal, 'restart dialog', timeout=120)
            index = next(i for i, x in enumerate(dialog['rows']) if x.startswith('Restart without saving:'))
            keys.key('Home')
            for _ in range(index):
                keys.key('Down')
            keys.key('Return')
            try:
                result['relaunch_line'] = starts.get(timeout=args.timeout).strip()
            except queue.Empty:
                raise AssertionError(f'the app did not relaunch within {args.timeout} s') from None
            result['after'] = json.loads(cli('versions'))
            result['host_instance_before'] = first
            record('Restart without saving finished and the app relaunched on daemon '
                   + str(result['after'].get('daemon', {}).get('version')))
        result['status'] = 'PASS'
    except BaseException as error:
        result['error'] = f'{type(error).__name__}: {error}'
        try:
            # What the window showed when the step gave up.
            result['last_window'] = window()
        except Exception as detail:
            result['last_window'] = f'unavailable: {detail}'
        raise
    finally:
        # A relaunched app and a restarted daemon are new processes; look each
        # up on its own so one failed lookup cannot skip the other.
        for lookup in (lambda: json.loads(cli('render-status', '--json')).get('host_pid'),
                       lambda: json.loads(cli('versions'))['daemon'].get('pid')):
            try:
                pid = lookup()
                if pid:
                    os.kill(pid, signal.SIGTERM)
            except Exception as detail:
                result.setdefault('cleanup_errors', []).append(str(detail))
        for process in reversed(owned):
            if process.poll() is None:
                process.terminate()
        for process in owned:
            try:
                process.wait(10)
            except Exception:
                process.kill()
        (out / 'result.json').write_text(json.dumps(result, indent=2))
        shutil.rmtree(runtime, ignore_errors=True)
    print(json.dumps({'status': result['status'], 'checks': len(result['checks'])}))
    return 0


if __name__ == '__main__':
    sys.exit(main())
