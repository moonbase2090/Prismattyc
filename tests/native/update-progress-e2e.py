#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Drive Update and restart in a real window and record its progress meter.

Disposable machines only. A full run replaces the installed Prismattyc with the
newest public release, exactly as a user's update would.

Linux and Windows: the binaries in --bins are first copied into OUT/install,
because the updater refuses to replace a build tree, and the host runs from
there. Their version must be older than the newest release. macOS: pass the
installed app's Contents/MacOS as --bins, with the branch binaries (built as an
older version) placed there, so the app updates itself as a user's would.

--fail-download NAME (Linux) puts a curl wrapper first on PATH that cuts the
download of the release file for component NAME short. The dialog must then
hold step 1 with the error and offer Check again or Close.

The drive waits on signals, never on fixed time or polling:
- pmuxd prints its socket path when it listens;
- with PRISMATTYC_UI_EVENTS=1 the app prints a line after a presented frame in
  which the update panel or the dialog changed state (positions and states
  only); every key the drive sends is followed by waiting for that line;
- the app prints its update steps ("update progress: ...") and its font line
  at every start, so a second font line is the relaunch.
Timeouts only bound a hung run. Rows and stage order are checked from the
rendered rows (render status, read after the matching line) and from the step
lines, not from the change lines. Every presented frame is kept as a numbered
file (PRISMATTYC_DUMP_PRESENT with {seq}); the frame named by each step's change
line is copied as that step's capture.

Keys go to the host only: Quartz events to its PID on macOS, XTEST through
xdotool on Linux, SendInput on Windows. --key-relay DIR (a guest whose privacy
settings forbid event injection) sends each key as a JSON line into the named
pipe DIR/requests and blocks on a line from DIR/acks, answered by an outside
input source such as the VM's console.

  update-progress-e2e.py --bins target/release --out DIR [--fail-download pmux-attach]
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

FAILED = 'Failed at step '
BINARIES = ('pmux', 'pmuxd', 'pmux-attach', 'pmux-mcp', 'prismattyc', 'prismattyc-host')
EXE = '.exe' if os.name == 'nt' else ''
# No Option/Alt: on macOS, Option+letter can be a dead key.
BINDING = 'ctrl+shift+y'
UI = 'prismattyc-host: ui: '
STEP = 'prismattyc-host: update progress: '
FONT = 'prismattyc-host: font '


class Keys:
    """Native key input for one host process."""

    # Each name maps to (macOS key code, X keysym, Windows virtual key).
    CODES = {'y': (16, 'y', 0x59), 'Home': (115, 'Home', 0x24), 'Down': (125, 'Down', 0x28),
             'Up': (126, 'Up', 0x26), 'Return': (36, 'Return', 0x0D), 'Escape': (53, 'Escape', 0x1B)}

    def __init__(self, pid, relay=None, relay_timeout=60):
        self.pid = pid
        self.relay = relay
        self.relay_timeout = relay_timeout
        if relay:
            relay.mkdir(parents=True, exist_ok=True)
            for name in ('requests', 'acks'):
                try:
                    os.mkfifo(relay / name)
                except FileExistsError:
                    pass
            # Open order matches the relay: requests first, then acks, so
            # each blocking open meets its peer. A relay that never attaches
            # fails at the bound.
            self.requests = bounded(lambda: (relay / 'requests').open('w'), relay_timeout,
                                    'key relay attaching to requests')
            self.acks = bounded(lambda: (relay / 'acks').open('r'), relay_timeout,
                                'key relay attaching to acks')
        elif sys.platform == 'darwin':
            import ctypes
            import ctypes.util
            self.quartz = ctypes.CDLL(ctypes.util.find_library('ApplicationServices'))
            self.quartz.CGEventCreateKeyboardEvent.restype = ctypes.c_void_p
            self.quartz.CGEventCreateKeyboardEvent.argtypes = [ctypes.c_void_p, ctypes.c_uint16, ctypes.c_bool]
            self.quartz.CGEventSetFlags.argtypes = [ctypes.c_void_p, ctypes.c_uint64]
            self.quartz.CGEventPostToPid.argtypes = [ctypes.c_int, ctypes.c_void_p]
            self.quartz.CFRelease.argtypes = [ctypes.c_void_p]

    def press(self, name, ctrl=False, shift=False):
        mac, keysym, vk = self.CODES[name]
        if self.relay:
            def send():
                self.requests.write(json.dumps({'key': name, 'ctrl': ctrl, 'shift': shift}) + '\n')
                self.requests.flush()
                return self.acks.readline()

            # A relay that stops answering fails at the bound; one that closes
            # fails at once (end of file, or a broken pipe on write).
            assert bounded(send, self.relay_timeout, f'key relay ack for {name}'), \
                f'the key relay closed before {name}'
        elif sys.platform == 'darwin':
            flags = (1 << 18 if ctrl else 0) | (1 << 17 if shift else 0)
            for down in (True, False):
                event = self.quartz.CGEventCreateKeyboardEvent(None, mac, down)
                assert event
                self.quartz.CGEventSetFlags(event, flags)
                self.quartz.CGEventPostToPid(self.pid, event)
                self.quartz.CFRelease(event)
        elif sys.platform.startswith('linux'):
            window = subprocess.check_output(['xdotool', 'search', '--sync', '--onlyvisible', '--pid',
                                              str(self.pid)], text=True).split()[0]
            subprocess.check_call(['xdotool', 'windowfocus', '--sync', window])
            chord = '+'.join([*(['ctrl'] if ctrl else []), *(['shift'] if shift else []), keysym])
            subprocess.check_call(['xdotool', 'key', '--clearmodifiers', chord])
        else:
            self._windows(vk, ctrl, shift)

    def close(self):
        # A thread still blocked on a stalled relay holds these files; the
        # process exit releases them.
        if self.relay:
            for stream in (self.requests, self.acks):
                try:
                    stream.close()
                except OSError:
                    pass

    def _windows(self, vk, ctrl, shift):
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

        class KEYBDINPUT(ctypes.Structure):
            _fields_ = [('wVk', wintypes.WORD), ('wScan', wintypes.WORD), ('dwFlags', wintypes.DWORD),
                        ('time', wintypes.DWORD), ('dwExtraInfo', ctypes.c_size_t)]

        class INPUT(ctypes.Structure):
            class _U(ctypes.Union):
                _fields_ = [('ki', KEYBDINPUT), ('pad', ctypes.c_byte * 32)]
            _anonymous_ = ('u',)
            _fields_ = [('type', wintypes.DWORD), ('u', _U)]

        keys = [*([0x11] if ctrl else []), *([0x10] if shift else []), vk]
        events = [(k, 0) for k in keys] + [(k, 2) for k in reversed(keys)]
        array = (INPUT * len(events))()
        for slot, (key, flags) in zip(array, events):
            slot.type = 1
            slot.ki = KEYBDINPUT(key, 0, flags, 0, 0)
        sent = user32.SendInput(len(events), array, ctypes.sizeof(INPUT))
        assert sent == len(events), f'SendInput sent {sent} of {len(events)}'


def bounded(step, timeout, label):
    """Run a blocking step on a helper thread and wait for it at most
    `timeout` seconds; the wait ends when the step returns or raises."""
    done = queue.Queue()

    def run():
        try:
            done.put((True, step()))
        except BaseException as error:
            done.put((False, error))

    threading.Thread(target=run, daemon=True).start()
    try:
        ok, value = done.get(timeout=timeout)
    except queue.Empty:
        raise AssertionError(f'no {label} within {timeout} s') from None
    if not ok:
        raise value
    return value


class LineReader:
    """A process's output read on a thread into a log file and a queue."""

    def __init__(self, process, log_path, keep):
        self.lines = queue.Queue()
        self.kept = []
        self.thread = threading.Thread(target=self._read, args=(process, log_path, keep), daemon=True)
        self.thread.start()

    def _read(self, process, log_path, keep):
        with log_path.open('w') as log:
            for line in process.stdout:
                log.write(line)
                log.flush()
                if keep(line):
                    self.kept.append(line)
                    self.lines.put(line)

    def await_line(self, test, label, timeout):
        """The next kept line passing `test`. `timeout` bounds the whole wait
        for a hung process, however many other lines arrive meanwhile; the
        wait ends on the line."""
        deadline = time.monotonic() + timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise AssertionError(f'no {label} within {timeout} s')
            try:
                line = self.lines.get(timeout=remaining)
            except queue.Empty:
                raise AssertionError(f'no {label} within {timeout} s') from None
            if test(line):
                return line


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--bins', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--fail-download', choices=BINARIES)
    parser.add_argument('--timeout', type=int, default=600)
    parser.add_argument('--key-relay', type=Path)
    args = parser.parse_args()
    assert not (args.fail_download and not sys.platform.startswith('linux')), '--fail-download is Linux only'
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=False)
    frames = out / 'frames'
    frames.mkdir()
    for name in ('home', 'config', 'data', 'state', 'project'):
        (out / name).mkdir()
    if sys.platform == 'darwin':
        bins = args.bins.resolve()
    else:
        bins = out / 'install' / 'bin'
        bins.mkdir(parents=True)
        for name in BINARIES:
            shutil.copy2(args.bins / (name + EXE), bins / (name + EXE))
    runtime = Path(tempfile.mkdtemp(prefix='pupd-'))
    config = out / 'config.toml'
    config.write_text('splash = false\nstart_at_login = false\nautomatic_update_checks = false\n'
                      'install_agent_skills = false\nchrome_style = "graphite"\n'
                      f'[keys]\nupdate_restart = "{BINDING}"\n')
    env = {k: v for k, v in os.environ.items() if not k.startswith(('PMUX', 'PRISMATTYC_', 'XDG_'))}
    # Windows requires a socket directory that pmux secures itself, so there the
    # private LOCALAPPDATA below holds the default socket.
    socket_path = None if os.name == 'nt' else runtime / 'pmux.sock'
    env.update(PMUX=str(bins / ('pmux' + EXE)), PRISMATTYC_CONFIG=str(config), PRISMATTYC_NO_AGENT_SKILLS='1',
               PRISMATTYC_UI_EVENTS='1', PRISMATTYC_DUMP_PRESENT=str(frames / 'frame-{seq}.png'))
    if socket_path:
        env['PMUX_SOCKET'] = str(socket_path)
    if os.name == 'nt':
        env.update(USERPROFILE=str(out / 'home'), LOCALAPPDATA=str(out / 'data'), APPDATA=str(out / 'config'))
    else:
        env.update(HOME=str(out / 'home'), SHELL='/bin/sh', XDG_RUNTIME_DIR=str(runtime),
                   XDG_DATA_HOME=str(out / 'data'), XDG_CONFIG_HOME=str(out / 'config'),
                   XDG_STATE_HOME=str(out / 'state'))
    env['PATH'] = str(bins) + os.pathsep + env['PATH']
    if args.fail_download:
        shim = out / 'curl-shim'
        shim.mkdir()
        real = shutil.which('curl')
        # The named release file arrives slowly and curl gives up on it after
        # three seconds, so the meter shows part of it before the failure.
        (shim / 'curl').write_text(f'''#!/bin/sh
for arg in "$@"; do
  case "$arg" in
    */prismattyc-v*-{args.fail_download}) exec {real} "$@" --limit-rate 300k --max-time 3 ;;
  esac
done
exec {real} "$@"
''')
        (shim / 'curl').chmod(0o755)
        env['PATH'] = str(shim) + os.pathsep + env['PATH']

    result = {'status': 'FAIL', 'mode': 'fail-download' if args.fail_download else 'update',
              'platform': sys.platform, 'checks': []}
    keys = app = None
    owned, readers, events = [], [], []

    def cli(*words, timeout=60):
        socket = ['--socket', str(socket_path)] if socket_path else []
        p = subprocess.run([str(bins / ('pmux' + EXE)), *socket, *words], env=env, cwd=out / 'project',
                           capture_output=True, text=True, timeout=timeout)
        if p.returncode:
            raise AssertionError(f'pmux {" ".join(words)} exited {p.returncode}: {p.stdout}{p.stderr}')
        return p.stdout

    def window():
        return json.loads(cli('render-status', '--json'))['windows'][0]

    def record(check):
        result['checks'].append(check)
        print('PASS ' + check, flush=True)

    def change(test, label, timeout=60):
        """The next change line whose state passes `test`."""
        line = app.await_line(lambda line: line.startswith(UI) and test(json.loads(line[len(UI):])['ui']),
                              label, timeout)
        event = json.loads(line[len(UI):])
        events.append(event)
        return event

    def select(rows, selected, label, position):
        """Move the selection to the row starting with `label`, one key at a
        time, waiting for each move to be shown; then Enter."""
        matches = [i for i, row in enumerate(rows) if row.startswith(label + ':') or row == label]
        assert matches, f'no {label!r} row in {rows}'
        target = matches[0]
        while selected != target:
            key = 'Down' if target > selected else 'Up'
            before = selected
            keys.press(key)
            moved = change(lambda ui: position(ui) not in (None, before), f'{key} toward {label}')
            selected = position(moved['ui'])
        keys.press('Return')

    def panel_selected(ui):
        return (ui.get('panel') or {}).get('selected')

    def dialog_state(ui):
        return (ui.get('dialog') or {}).get('state')

    def dialog_selected(ui):
        return (ui.get('dialog') or {}).get('selected')

    try:
        result['update_check'] = json.loads(subprocess.run(
            [str(bins / ('pmux' + EXE)), 'update', '--check', '--json'], env=env, capture_output=True,
            text=True, timeout=120).stdout or '{}')
        assert result['update_check'].get('update_available'), result['update_check']
        socket = ['--socket', str(socket_path)] if socket_path else []
        daemon = subprocess.Popen([str(bins / ('pmuxd' + EXE)), *socket], env=env, cwd=out / 'project',
                                  stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, errors='replace')
        owned.append(daemon)
        daemon_out = LineReader(daemon, out / 'daemon.log', lambda line: True)
        readers.append(daemon_out)
        daemon_out.await_line(lambda line: line.strip().endswith('pmux.sock'), 'daemon socket line', 60)
        cli('new', '--no-attach', 'work')
        host = subprocess.Popen([str(bins / ('prismattyc-host' + EXE)), '--no-splash', '--attach-session', 'work'],
                                env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
                                errors='replace')
        owned.append(host)
        app = LineReader(host, out / 'host.log', lambda line: line.startswith((UI, STEP, FONT)))
        readers.append(app)
        change(lambda ui: True, 'first presented frame')
        keys = Keys(host.pid, args.key_relay)
        keys.press('y', ctrl=True, shift=True)
        opened = change(lambda ui: (ui.get('panel') or {}).get('page') == 'maintenance'
                        and panel_selected(ui) is not None, 'update panel')
        rows = [row.removesuffix(': ') for row in (window().get('space_details') or {}).get('rows', [])]
        select(rows, panel_selected(opened['ui']), 'Update and restart all components', panel_selected)
        consent = change(lambda ui: dialog_state(ui) == 'consent', 'restart dialog', 120)
        select(window()['restart_modal']['rows'], dialog_selected(consent['ui']), 'Restart without saving',
               dialog_selected)
        change(lambda ui: dialog_state(ui) == 'running', 'progress view')
        if args.fail_download:
            failed = change(lambda ui: dialog_state(ui) == 'failed', 'held failure', args.timeout)
            final = window()['restart_modal']['rows']
            result['final_rows'] = final
            result['failure_frame'] = failed['seq']
            assert final[0] == 'Failed at step 1 of 4: Download', final
            assert final[1].startswith(f'Downloading {args.fail_download}'), final
            assert f'download failed for {args.fail_download}' in ' '.join(final), final
            assert final[-2].startswith('Check again') and final[-1].startswith('Close'), final
            record('the dialog held step 1 of 4 with the download error and offered Check again or Close')
        else:
            result['relaunch_line'] = app.await_line(lambda line: line.startswith(FONT), 'relaunched app',
                                                     args.timeout).strip()
            result['after'] = json.loads(cli('versions'))
            record('update finished and the app relaunched on daemon '
                   + str(result['after'].get('daemon', {}).get('version')))
    except BaseException as error:
        result['error'] = f'{type(error).__name__}: {error}'
        raise
    finally:
        if keys:
            keys.close()
        steps = [line[len(STEP):].strip() for line in (app.kept if app else []) if line.startswith(STEP)]
        result['step_log'] = steps
        stages = []
        for line in steps:
            stage = line.split(' | ', 1)[0].split(': ', 1)[1]
            if not stages or stages[-1] != stage:
                stages.append(stage)
        result['stages'] = stages
        (out / 'events.jsonl').write_text(''.join(json.dumps(event) + '\n' for event in events))
        # Each step's capture is the frame its change line was printed after.
        captures = {}
        for line in (app.kept if app else []):
            if not line.startswith(UI):
                continue
            event = json.loads(line[len(UI):])
            dialog = event['ui'].get('dialog') or {}
            if dialog.get('step'):
                captures.setdefault(f"{dialog['state']}-step{dialog['step'][0]}", event['seq'])
        for name, seq in captures.items():
            frame = frames / f'frame-{seq:06d}.png'
            if frame.exists():
                shutil.copy(frame, out / f'{name}.png')
        result['captures'] = captures
        if 'error' not in result:
            expected = ['Download'] if args.fail_download else ['Download', 'Verify', 'Install', 'Restart']
            downloads = [line for line in steps if line.startswith('Step 1 of 4: Download')]
            failed = [line for line in steps if line.startswith(FAILED)]
            if stages == expected and len(downloads) >= 2 and bool(failed) == bool(args.fail_download):
                result['status'] = 'PASS'
                record('stages in order: ' + ' > '.join(stages))
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
        for process in owned:
            if process.poll() is None:
                process.terminate()
        for process in owned:
            try:
                process.wait(10)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
        for reader in readers:
            reader.thread.join(10)
        (out / 'result.json').write_text(json.dumps(result, indent=2))
        shutil.rmtree(runtime, ignore_errors=True)
    print(json.dumps({k: result[k] for k in ('status', 'stages')}))
    return 0 if result['status'] == 'PASS' else 1


if __name__ == '__main__':
    sys.exit(main())
