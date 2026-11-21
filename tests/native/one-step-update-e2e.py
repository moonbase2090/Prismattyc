#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Prove the one-step update on a clean machine.

The machine has an older public release installed by its own installer, with
this build's `pmux`, built as that release, in place of the installed one (a
user on a release that carries this change). The newest public release is
newer. The drive then checks:

* --notice (Linux, Windows): this build's app shows the update notice; a click
  on it opens the Restart to update review, and Escape closes it again with
  nothing installed.
* `pmux update` from this build installs the newest release, includes its
  monitor launcher (Linux, Windows; macOS carries it in the app), restarts the
  open app window, leaves the running sessions alone (the daemon is deferred
  with a next action) and prints one receipt.
* A second `pmux update --json --restart` finds everything current except the
  deferred daemon.

Disposable machines only: it replaces the installed release.
"""
import argparse
import importlib.util
import json
import os
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
_spec = importlib.util.spec_from_file_location('update_progress_e2e', HERE / 'update-progress-e2e.py')
_drive = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_drive)
Keys, LineReader, EXE = _drive.Keys, _drive.LineReader, _drive.EXE

UI = 'prismattyc-host: ui: '
# A program that keeps a session busy until the machine is thrown away.
BUSY = [sys.executable, '-c', 'import time\nwhile True: time.sleep(3600)']


def version_in(label):
    """The first x.y.z word of a version label such as `pmux 0.3.72 (abc)`."""
    for word in label.split():
        parts = word.lstrip('v').split('.')
        if len(parts) == 3 and all(part.isdecimal() for part in parts):
            return word.lstrip('v')
    raise AssertionError(f'no version in {label!r}')


def click(pid, x, y, timeout=60):
    """A left click at (x, y) device pixels inside the window of `pid`."""
    if sys.platform.startswith('linux'):
        window = subprocess.check_output(['xdotool', 'search', '--sync', '--onlyvisible', '--pid', str(pid)],
                                         text=True, timeout=timeout).split()[0]
        subprocess.check_call(['xdotool', 'windowfocus', '--sync', window], timeout=timeout)
        subprocess.check_call(['xdotool', 'mousemove', '--window', window, str(x), str(y), 'click', '1'],
                              timeout=timeout)
        return
    assert os.name == 'nt', 'the notice is shown on Linux and Windows'
    import ctypes
    from ctypes import wintypes
    user32 = ctypes.WinDLL('user32', use_last_error=True)
    found = []

    @ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)
    def visit(hwnd, _):
        owner = wintypes.DWORD()
        user32.GetWindowThreadProcessId(hwnd, ctypes.byref(owner))
        if owner.value == pid and user32.IsWindowVisible(hwnd):
            found.append(hwnd)
        return True

    user32.EnumWindows(visit, 0)
    assert found, 'host window not found'
    user32.SetForegroundWindow(found[0])
    point = wintypes.POINT(x, y)
    assert user32.ClientToScreen(found[0], ctypes.byref(point))
    assert user32.SetCursorPos(point.x, point.y)

    class MOUSEINPUT(ctypes.Structure):
        _fields_ = [('dx', wintypes.LONG), ('dy', wintypes.LONG), ('mouseData', wintypes.DWORD),
                    ('dwFlags', wintypes.DWORD), ('time', wintypes.DWORD), ('dwExtraInfo', ctypes.c_size_t)]

    class INPUT(ctypes.Structure):
        class _U(ctypes.Union):
            _fields_ = [('mi', MOUSEINPUT), ('pad', ctypes.c_byte * 32)]
        _anonymous_ = ('u',)
        _fields_ = [('type', wintypes.DWORD), ('u', _U)]

    events = (INPUT * 2)()
    for slot, flags in zip(events, (0x0002, 0x0004)):  # left down, left up
        slot.type = 0
        slot.mi = MOUSEINPUT(0, 0, 0, flags, 0, 0)
    sent = user32.SendInput(2, events, ctypes.sizeof(INPUT))
    assert sent == 2, f'SendInput sent {sent} of 2'


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--pmux', type=Path, required=True,
                        help='the installed pmux, this build built as the installed release')
    parser.add_argument('--notice-host', type=Path,
                        help='this build\'s prismattyc-host, for --notice')
    parser.add_argument('--again-pmux', type=Path,
                        help='a pmux with this change for the second update (default: --pmux)')
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--notice', action='store_true', help='also click the update notice (Linux, Windows)')
    parser.add_argument('--screenshots', action='store_true')
    parser.add_argument('--key-relay', type=Path, help='relay directory for screenshots taken outside (macOS)')
    args = parser.parse_args()
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=False)
    installed_pmux = args.pmux.absolute()
    installed_host = installed_pmux.with_name('prismattyc-host' + EXE)
    again_pmux = (args.again_pmux or args.pmux).absolute()
    env = dict(os.environ)
    checks, records = [], []
    relay = Keys(0, args.key_relay) if args.key_relay else None

    def passed(check):
        checks.append(check)
        print('PASS ' + check, flush=True)

    def screenshot(name):
        if not args.screenshots:
            return
        path = out / f'screen-{name}.png'
        if relay:
            request = (json.dumps({'snap': name}) + '\n').encode()
            assert os.write(relay.requests, request) == len(request), f'short relay write for {name}'
            relay._await_ack(name)
            return
        if sys.platform == 'darwin':
            command = ['screencapture', '-x', str(path)]
        elif sys.platform.startswith('linux'):
            command = ['import', '-window', 'root', str(path)]
        else:
            command = ['powershell', '-NoProfile', '-Command',
                       'Add-Type -AssemblyName System.Windows.Forms,System.Drawing;'
                       '$b=[System.Windows.Forms.SystemInformation]::VirtualScreen;'
                       '$i=New-Object System.Drawing.Bitmap $b.Width,$b.Height;'
                       '$g=[System.Drawing.Graphics]::FromImage($i);'
                       '$g.CopyFromScreen($b.Left,$b.Top,0,0,$i.Size);'
                       f'$i.Save("{path}",[System.Drawing.Imaging.ImageFormat]::Png)']
        subprocess.run(command, check=True, timeout=60)

    def run(program, *words, timeout=120, check=True):
        result = subprocess.run([str(program), *words], env=env, capture_output=True, text=True,
                                encoding='utf-8', errors='replace', timeout=timeout)
        records.append({'command': [Path(program).name, *words], 'exit': result.returncode,
                        'stdout': result.stdout, 'stderr': result.stderr})
        if check:
            assert result.returncode == 0, records[-1]
        return result

    def versions(pmux):
        return json.loads(run(pmux, 'versions').stdout)

    def await_versions(pmux, test, label, timeout=90):
        """Poll `pmux versions` until `test` holds; the deadline fails the run."""
        deadline = time.monotonic() + timeout
        while True:
            report = versions(pmux)
            if test(report):
                return report
            assert time.monotonic() < deadline, f'{label} within {timeout} s: {report}'
            time.sleep(0.5)

    old = version_in(run(installed_pmux, '--version').stdout)
    check = json.loads(run(installed_pmux, 'update', '--check', '--json').stdout)
    latest = check['available']
    assert check['update_available'], f'the newest release must be newer than {old}: {check}'
    before = versions(installed_pmux)
    assert {version_in(item['version'] or '') for item in before['installed']} == {old}, before
    passed(f'installed release {old}, newest release {latest}')

    for name in ('work-1', 'work-2'):
        run(installed_pmux, 'new', '--no-attach', name, '--', *BUSY)
    sessions = set(run(installed_pmux, 'ls').stdout.split())
    assert {'work-1', 'work-2'} <= sessions, sessions

    if args.notice:
        assert sys.platform.startswith('linux') or os.name == 'nt', 'the notice is shown on Linux and Windows'
        assert args.notice_host, '--notice needs --notice-host'
        host = subprocess.Popen([str(args.notice_host.resolve()), '--no-splash', '--attach-session', 'work-1'],
                                env=dict(env, PRISMATTYC_UI_EVENTS='1'), stdout=subprocess.PIPE,
                                stderr=subprocess.STDOUT, text=True, errors='replace')
        app = LineReader(host, out / 'notice-host.log', lambda line: line.startswith(UI))

        def change(test, label, timeout=90):
            line = app.await_line(lambda line: test(json.loads(line[len(UI):])['ui']), label, timeout)
            return json.loads(line[len(UI):])['ui']

        try:
            ui = change(lambda ui: any(t['opens_update'] for t in ui.get('toasts') or []), 'the update notice')
            notice = next(t for t in ui['toasts'] if t['opens_update'])
            screenshot('notice')
            x, y, w, h = notice['rect']
            click(host.pid, x + w // 2, y + h // 2)
            change(lambda ui: ui.get('dialog') is not None and ui.get('modal') == 'restart',
                   'the Restart to update review')
            screenshot('notice-review')
            Keys(host.pid).press('Escape')
            change(lambda ui: ui.get('dialog') is None and ui.get('modal') is None, 'the review closed')
            assert version_in(run(installed_pmux, '--version').stdout) == old, 'the notice installed something'
            passed('a click on the update notice opens the Restart to update review; Escape closes it')
        finally:
            host.terminate()
            host.wait(30)

    # The installed older app, open on a session, so the update restarts it.
    app_host = subprocess.Popen([str(installed_host), '--no-splash', '--attach-session', 'work-2'], env=env,
                                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    await_versions(installed_pmux, lambda report: any(item.get('component') == 'host' and
                                                      item.get('pid') == app_host.pid
                                                      for item in report['running']),
                   'the installed app registered')
    screenshot('before-update')

    update = run(installed_pmux, 'update', timeout=900, check=False)
    (out / 'receipt.txt').write_text(update.stdout)
    (out / 'update-stderr.txt').write_text(update.stderr)
    print(update.stdout, flush=True)
    assert update.returncode == 0, records[-1]
    lines = update.stdout.splitlines()
    assert lines[0] == f'Updated Prismattyc {old} to {latest}.', lines
    assert f'  Core: installed {latest}' in lines, lines
    launcher = 'included in Prismattyc.app' if sys.platform == 'darwin' else f'installed {latest}'
    assert f'  Monitor launcher: {launcher}' in lines, lines
    assert any(line.startswith('  App window ') and line.endswith(': restarted') for line in lines), lines
    assert any(line.startswith('  Daemon: deferred') for line in lines), lines
    nexts = lines[lines.index('Next:') + 1:]
    assert any(line.startswith('  - Daemon: ') and '--stop-sessions' in line for line in nexts), lines
    passed('one receipt: core installed, launcher included, app window restarted, daemon deferred with its next step')

    # The installed CLI is now the new release's.
    new_pmux = installed_pmux
    after = await_versions(new_pmux, lambda report: any(
        item.get('component') == 'host' and version_in(item.get('version') or '0') == latest
        for item in report['running']), 'the app window back on the new release')
    assert {version_in(item['version'] or '') for item in after['installed']} == {latest}, after
    stale_hosts = [item for item in after['running']
                   if item.get('component') == 'host' and version_in(item.get('version') or '0') != latest]
    assert not stale_hosts, after
    assert version_in(after['daemon'].get('version') or '') == old, 'the daemon keeps the sessions on its release'
    assert {'work-1', 'work-2'} <= set(run(new_pmux, 'ls').stdout.split()), 'sessions keep running'
    if sys.platform != 'darwin':
        monitor = installed_pmux.with_name('pmux-activity-monitor' + EXE)
        assert version_in(run(monitor, '--version').stdout) == latest, 'the launcher matches the new core'
    passed(f'every installed component and the open app window run {latest}; the sessions keep running')
    screenshot('after-update')

    # The installed pmux is now the newest public release's; a release that
    # carries this change runs the second update.
    again = run(again_pmux, 'update', '--json', '--restart', timeout=600)
    receipt = json.loads(again.stdout.splitlines()[-1])
    (out / 'receipt-again.json').write_text(json.dumps(receipt, indent=2) + '\n')
    assert receipt['core'] == {'status': 'current', 'version': latest}, receipt
    assert receipt['monitor_launcher']['status'] in ('current', 'in_app'), receipt
    assert receipt['restart']['status'] == 'ran', receipt
    daemon = [c for c in receipt['restart']['components'] if c['component'] == 'daemon']
    assert daemon and daemon[0]['status'] == 'deferred' and daemon[0]['next'], receipt
    assert receipt['status'] == 'updated_with_deferrals', receipt
    passed('a second update finds the release and launcher current and only the daemon still deferred')

    app_host.terminate()
    if relay:
        relay.close()
    (out / 'result.json').write_text(json.dumps({'status': 'PASS', 'from': old, 'to': latest, 'checks': checks,
                                                 'records': records}, indent=2) + '\n')
    print(f'PASS one-step update {old} -> {latest} ({len(checks)} checks)', flush=True)


if __name__ == '__main__':
    main()
