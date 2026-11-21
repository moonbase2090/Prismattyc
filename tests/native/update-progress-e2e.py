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

--save chooses Save and restart and checks that the sessions come back. Before
the update it opens two tabs: a plain session and one whose window is split in
two panes, and prints a one-time marker into every pane. After the relaunch the
sessions must have the same session and pane IDs, every marker must be in its
pane's scrollback, and the app must show the same tabs. It writes before/after
state (before.json, after.json) and captures (before.png, after-notice.png
once the receipt is closed, after.png).

Every successful run also checks the progress across the relaunch: the
restart's steps (from `pmux restart --progress` and the app's own) appear in
order in the step lines of both apps, and the relaunched app finishes the
meter on "Opening the windows" and shows a receipt with every step done
(after-receipt.png), which Escape closes before the notice. The restart code runs from the installed release, so to test a
branch's restart, build it as the newest version: the flow is then Restart
components (no download), which runs the same save and restore.

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
settings forbid event injection) writes each key as a JSON line into the named
pipe DIR/requests and waits for a line on DIR/acks, answered by an outside
input source such as the VM's console. The drive holds both pipes open
read-write and non-blocking and waits for each ack with select, so an absent,
stalled or closed relay fails at the per-key bound and cleanup never blocks.

  update-progress-e2e.py --bins target/release --out DIR [--fail-download pmux-attach]
"""
import argparse
import json
import os
import queue
import select
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

    def __init__(self, pid, relay=None, timeout=60):
        self.pid = pid
        self.relay = relay
        # Bounds each key's relay step or desktop call.
        self.relay_timeout = timeout
        if relay:
            relay.mkdir(parents=True, exist_ok=True)
            for name in ('requests', 'acks'):
                try:
                    os.mkfifo(relay / name)
                except FileExistsError:
                    pass
            # Both ends open read-write and non-blocking: the opens never wait
            # for the relay, a request (well under the pipe's atomic size)
            # never blocks, and nothing here can block a later close. The
            # only wait is for each ack, under `timeout`.
            flags = os.O_RDWR | os.O_NONBLOCK
            self.requests = os.open(relay / 'requests', flags)
            self.acks = os.open(relay / 'acks', flags)
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
            request = (json.dumps({'key': name, 'ctrl': ctrl, 'shift': shift}) + '\n').encode()
            assert os.write(self.requests, request) == len(request), f'short relay write for {name}'
            self._await_ack(name)
        elif sys.platform == 'darwin':
            flags = (1 << 18 if ctrl else 0) | (1 << 17 if shift else 0)
            for down in (True, False):
                event = self.quartz.CGEventCreateKeyboardEvent(None, mac, down)
                assert event
                self.quartz.CGEventSetFlags(event, flags)
                self.quartz.CGEventPostToPid(self.pid, event)
                self.quartz.CFRelease(event)
        elif sys.platform.startswith('linux'):
            # Each call is bounded so a stalled desktop fails the run.
            window = subprocess.check_output(['xdotool', 'search', '--sync', '--onlyvisible', '--pid',
                                              str(self.pid)], text=True, timeout=self.relay_timeout).split()[0]
            subprocess.check_call(['xdotool', 'windowfocus', '--sync', window], timeout=self.relay_timeout)
            chord = '+'.join([*(['ctrl'] if ctrl else []), *(['shift'] if shift else []), keysym])
            subprocess.check_call(['xdotool', 'key', '--clearmodifiers', chord], timeout=self.relay_timeout)
        else:
            self._windows(vk, ctrl, shift)

    def _await_ack(self, name):
        """Read one ack line, waiting at most the per-key timeout in total. A
        relay that is absent, stalled or gone fails at that bound."""
        deadline = time.monotonic() + self.relay_timeout
        line = b''
        while not line.endswith(b'\n'):
            remaining = deadline - time.monotonic()
            if remaining <= 0 or not select.select([self.acks], [], [], remaining)[0]:
                raise AssertionError(f'no key relay ack for {name} within {self.relay_timeout} s')
            try:
                line += os.read(self.acks, 256)
            except BlockingIOError:
                continue

    def close(self):
        if self.relay:
            os.close(self.requests)
            os.close(self.acks)

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
    parser.add_argument('--save', action='store_true', help='choose Save and restart and check the sessions return')
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
                      f'[keys]\nupdate_restart = "{BINDING}"\n'
                      # A known shell, so the marker command below is exact.
                      + ('[mux]\nshell = "cmd.exe"\n' if os.name == 'nt' else ''))
    env = {k: v for k, v in os.environ.items() if not k.startswith(('PMUX', 'PRISMATTYC_', 'XDG_'))}
    # Windows requires a socket directory that pmux secures itself, so there the
    # private LOCALAPPDATA below holds the default socket.
    # Unix: the instance directory under XDG_RUNTIME_DIR, where installs put
    # it; the daemon keeps pane scrollback only for sockets there.
    socket_path = None if os.name == 'nt' else runtime / 'prismattyc' / 'pmux.sock'
    if socket_path:
        socket_path.parent.mkdir(mode=0o700)
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

    def pane_ids():
        """{session name: [(session id, pane ids)]} from `pmux ls`."""
        found, current = {}, None
        for line in cli('ls').splitlines():
            words = line.split()
            if line.startswith('session '):
                current = words[1]
                found[current] = {'id': line.split('(id ')[1].split(')')[0], 'panes': []}
            elif words[:1] == ['pane'] and current:
                found[current]['panes'].append(words[1])
        return found

    def await_marker(session, pane, marker):
        """Wait until the daemon shows `marker` in the pane: a watching attach
        prints one line per screen revision."""
        socket = ['--socket', str(socket_path)] if socket_path else []
        watch = subprocess.Popen([str(bins / ('pmux' + EXE)), *socket, 'attach', session, '--watch', '--json',
                                  '--pane', pane], env=env, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                 text=True, errors='replace')
        reader = LineReader(watch, out / f'watch-{pane}.log', lambda line: marker in line)
        try:
            reader.await_line(lambda line: marker in line, f'marker in pane {pane}', 60)
        finally:
            watch.terminate()
            watch.wait(10)
            reader.thread.join(10)

    def host_state():
        return {'sessions': pane_ids(), 'tabs': json.loads(cli('tab', 'list', '--json'))}

    try:
        result['update_check'] = json.loads(subprocess.run(
            [str(bins / ('pmux' + EXE)), 'update', '--check', '--json'], env=env, capture_output=True,
            text=True, timeout=120).stdout or '{}')
        update = bool(result['update_check'].get('update_available'))
        assert update or args.save, result['update_check']
        socket = ['--socket', str(socket_path)] if socket_path else []
        daemon = subprocess.Popen([str(bins / ('pmuxd' + EXE)), *socket], env=env, cwd=out / 'project',
                                  stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, errors='replace')
        owned.append(daemon)
        daemon_out = LineReader(daemon, out / 'daemon.log', lambda line: True)
        readers.append(daemon_out)
        daemon_out.await_line(lambda line: line.strip().endswith('pmux.sock'), 'daemon socket line', 60)
        cli('new', '--no-attach', 'work')
        attach = ['--attach-session', 'work']
        markers = {}
        if args.save:
            # A second session whose window is split in two panes.
            cli('layout', 'save', 'work', '--name', 'split-view')
            layout = next(out.rglob('split-view.json'))
            saved = json.loads(layout.read_text())
            leaf = saved['windows'][0]['root']
            saved['session'] = 'split-view'
            saved['windows'][0]['root'] = {'kind': 'split', 'axis': 'vertical', 'ratio': 0.5,
                                           'first': leaf, 'second': dict(leaf)}
            layout.write_text(json.dumps(saved))
            cli('layout', 'apply', 'split-view')
            # The split tab first, so the before and after captures show it.
            attach = ['--attach-session', 'split-view', *attach]
            for session, found in pane_ids().items():
                for pane in found['panes'] if session in ('work', 'split-view') else []:
                    # The typed line differs from the printed one, so only
                    # the output (and later the scrollback) holds the marker.
                    nonce = f'{pane}x{os.getpid()}'
                    if os.name == 'nt':
                        typed, marker = f'echo KEEP-^{nonce}', f'KEEP-{nonce}'
                    else:
                        typed, marker = f'echo KEEP-$((7)){nonce}', f'KEEP-7{nonce}'
                    markers[pane] = (session, marker)
                    cli('send', pane, typed, '--enter')
                    await_marker(session, pane, marker)
            assert len(markers) == 3, markers
        host = subprocess.Popen([str(bins / ('prismattyc-host' + EXE)), '--no-splash', *attach],
                                env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
                                errors='replace')
        owned.append(host)
        app = LineReader(host, out / 'host.log', lambda line: line.startswith((UI, STEP, FONT)))
        readers.append(app)
        change(lambda ui: True, 'first presented frame')
        keys = Keys(host.pid, args.key_relay)
        if args.save:
            latest = max(frames.glob('frame-*.png'), default=None)
            if latest:
                shutil.copy(latest, out / 'before.png')
        keys.press('y', ctrl=True, shift=True)
        opened = change(lambda ui: (ui.get('panel') or {}).get('page') == 'maintenance'
                        and panel_selected(ui) is not None, 'update panel')
        rows = [row.removesuffix(': ') for row in (window().get('space_details') or {}).get('rows', [])]
        select(rows, panel_selected(opened['ui']), 'Update and restart all components', panel_selected)
        consent = change(lambda ui: dialog_state(ui) == 'consent', 'restart dialog', 120)
        if args.save:
            # Read here, after the app has drawn several frames, so its tab
            # list is published; right after the first frame it may not be.
            before = host_state()
            result['before'] = before
            (out / 'before.json').write_text(json.dumps(before, indent=2))
            assert any(view.get('tabs') for manifest in before['tabs'] for view in manifest.get('views', [])), before
        select(window()['restart_modal']['rows'], dialog_selected(consent['ui']),
               'Save and restart' if args.save else 'Restart without saving', dialog_selected)
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
            # The relaunched app numbers its frames from 1 again; keep the
            # first run's frames before it overwrites them.
            shutil.copytree(frames, out / 'frames-first')
            result['after'] = json.loads(cli('versions'))
            record('update finished and the app relaunched on daemon '
                   + str(result['after'].get('daemon', {}).get('version')))
            # The relaunched app shows the result notice and, over it, the
            # meter it carried, finished, with the receipt.
            notice = change(lambda ui: dialog_state(ui) == 'finished'
                            and (ui.get('panel') or {}).get('page') in ('update', 'result'),
                            'receipt and result', 120)
            shutil.copy(frames / f"frame-{notice['seq']:06d}.png", out / 'after-receipt.png')
            receipt = window()['restart_modal']
            result['receipt_rows'] = receipt['rows']
            result['notice_rows'] = (window().get('space_details') or {}).get('rows', [])
            count = 4 if result['update_check'].get('update_available') else 1
            assert receipt['rows'][0] == f'Finished step {count} of {count}: Restart', receipt['rows']
            assert receipt['rows'][1].startswith('Opening the windows ('), receipt['rows']
            done = receipt['running']['receipt']
            assert len(done) == count and all(line.endswith(': done') for line in done), done
            record(f'the relaunched app finished the meter on Opening the windows; receipt: {"; ".join(done)}')
            # On Windows the relaunched app is a new process; elsewhere it
            # replaced itself and the same key sender (and relay) still
            # reaches it.
            pid = json.loads(cli('render-status', '--json')).get('host_pid')
            if pid != host.pid:
                keys.close()
                keys = Keys(pid, args.key_relay)
            keys.press('Escape')
            closed = change(lambda ui: ui.get('dialog') is None and ui.get('panel') is not None,
                            'receipt closed')
            if args.save:
                shutil.copy(frames / f"frame-{closed['seq']:06d}.png", out / 'after-notice.png')
                after = host_state()
                result['after_state'] = after
                (out / 'after.json').write_text(json.dumps(after, indent=2))
                for name in ('work', 'split-view'):
                    assert after['sessions'].get(name) == before['sessions'][name], (name, before, after)
                record('both sessions came back with their session and pane IDs, including the split pane')
                for pane, (session, marker) in markers.items():
                    history = cli('save-buffer', pane, '-', '--history')
                    assert marker in history, (pane, marker, history[-2000:])
                record(f'all {len(markers)} scrollback markers are back in their panes')
                def titles(state):
                    # `tab list --json`: one manifest per app process, each
                    # with its windows (views) and their tabs.
                    return sorted(tab.get('title', '') for manifest in state['tabs']
                                  for view in manifest.get('views', []) for tab in view.get('tabs', []))
                assert titles(before) and titles(after) == titles(before), (before['tabs'], after['tabs'])
                record('the app reopened the same tabs without asking')
                keys.press('Escape')
                closed = change(lambda ui: ui.get('panel') is None, 'panel closed')
                shutil.copy(frames / f"frame-{closed['seq']:06d}.png", out / 'after.png')
    except BaseException as error:
        result['error'] = f'{type(error).__name__}: {error}'
        raise
    finally:
        if keys:
            keys.close()
        steps = [line[len(STEP):].strip() for line in (app.kept if app else []) if line.startswith(STEP)]
        result['step_log'] = steps
        stages = []
        def stage_of(line):
            return line.split(' | ', 1)[0].split(': ', 1)[1]
        for line in steps:
            stage = stage_of(line)
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
        first = out / 'frames-first' if (out / 'frames-first').is_dir() else frames
        for name, seq in captures.items():
            frame = first / f'frame-{seq:06d}.png'
            if frame.exists():
                shutil.copy(frame, out / f'{name}.png')
        result['captures'] = captures
        if 'error' not in result:
            update = result.get('update_check', {}).get('update_available')
            expected = (['Download'] if args.fail_download else
                        ['Download', 'Verify', 'Install', 'Restart'] if update else ['Restart'])
            downloads = [line for line in steps if line.startswith('Step 1 of 4: Download')]
            failed = [line for line in steps if line.startswith(FAILED)]
            # The restart's steps, from both apps, each once and in order.
            restart = [] if args.fail_download else (
                ['Saving the sessions', 'Stopping the old daemon', 'Starting the new daemon',
                 'Checking the restored sessions'] if args.save else
                ['Stopping the sessions', 'Starting the new daemon']) + [
                'Reconciling the official plug-ins', 'Restarting the MCP adapter',
                'Relaunching the app', 'Opening the windows']
            details = []
            for line in steps:
                detail = line.split(' | ', 1)[1] if ' | ' in line else ''
                if ' (' in detail and stage_of(line) == 'Restart' and detail not in details:
                    details.append(detail)
            wanted = [f'{item} ({index} of {len(restart)})' for index, item in enumerate(restart, 1)]
            result['restart_steps'] = details
            if (stages == expected and (len(downloads) >= 2 or not update)
                    and bool(failed) == bool(args.fail_download) and details == wanted):
                result['status'] = 'PASS'
                record('stages in order: ' + ' > '.join(stages))
                if restart:
                    record('restart steps in order across the relaunch: ' + ' > '.join(details))
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
