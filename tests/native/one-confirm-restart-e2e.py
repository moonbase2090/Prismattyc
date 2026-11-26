#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Record the one-confirm pmux server restart in a real window.

The host must be older than the newest public release. On macOS the automatic
check opens the update alert; Review Update then opens one dialog that lists
the running sessions. Save and restart is the first choice and Restart when
idle is the next. This drive chooses Restart when idle and checks that the
sessions keep running. It does not install a release or stop the daemon.

Keys and the alert button go only to the owned host process.

  one-confirm-restart-e2e.py --bins target/debug --out DIR
"""
import argparse
import ctypes as C
import json
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

assert sys.platform == 'darwin', 'this drive records the macOS update alert'


class Desktop:
    """Accessibility queries and key events for one host pid."""

    def __init__(self):
        self.ax = C.CDLL('/System/Library/Frameworks/ApplicationServices.framework/ApplicationServices')
        self.cf = C.CDLL('/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation')
        C.CDLL('/System/Library/Frameworks/AppKit.framework/AppKit')
        objc = C.CDLL('/usr/lib/libobjc.A.dylib')
        pointer = C.c_void_p
        objc.objc_getClass.argtypes = [C.c_char_p]
        objc.objc_getClass.restype = pointer
        objc.sel_registerName.argtypes = [C.c_char_p]
        objc.sel_registerName.restype = pointer
        self.cls = objc.objc_getClass
        self.sel = objc.sel_registerName
        self.msg = C.CFUNCTYPE(pointer, pointer, pointer)(('objc_msgSend', objc))
        self.msg_int = C.CFUNCTYPE(pointer, pointer, pointer, C.c_int)(('objc_msgSend', objc))
        self.msg_ulong = C.CFUNCTYPE(C.c_bool, pointer, pointer, C.c_ulong)(('objc_msgSend', objc))
        for fn, args, result in [
            (self.ax.AXIsProcessTrusted, [], C.c_bool),
            (self.ax.AXUIElementCreateApplication, [C.c_int], pointer),
            (self.ax.AXUIElementCopyAttributeValue, [pointer, pointer, C.POINTER(pointer)], C.c_int),
            (self.ax.AXUIElementPerformAction, [pointer, pointer], C.c_int),
            (self.ax.CGEventCreateKeyboardEvent, [pointer, C.c_uint16, C.c_bool], pointer),
            (self.ax.CGEventSetFlags, [pointer, C.c_uint64], None),
            (self.ax.CGEventPostToPid, [C.c_int, pointer], None),
            (self.cf.CFStringCreateWithCString, [pointer, C.c_char_p, C.c_uint32], pointer),
            (self.cf.CFStringGetCString, [pointer, pointer, C.c_long, C.c_uint32], C.c_bool),
            (self.cf.CFArrayGetCount, [pointer], C.c_long),
            (self.cf.CFArrayGetValueAtIndex, [pointer, C.c_long], pointer),
            (self.cf.CFRelease, [pointer], None),
            (self.cf.CFRetain, [pointer], pointer),
            (self.cf.CFGetTypeID, [pointer], C.c_ulong),
            (self.cf.CFStringGetTypeID, [], C.c_ulong),
        ]:
            fn.argtypes = args
            fn.restype = result
        if not self.ax.AXIsProcessTrusted():
            raise RuntimeError('Accessibility permission is required')

    def string(self, text):
        return self.cf.CFStringCreateWithCString(None, text.encode(), 0x08000100)

    def text(self, value):
        if not value or self.cf.CFGetTypeID(value) != self.cf.CFStringGetTypeID():
            return ''
        buf = C.create_string_buffer(4096)
        if not self.cf.CFStringGetCString(value, buf, len(buf), 0x08000100):
            return ''
        return buf.value.decode()

    def attr(self, node, key):
        name = self.string(key)
        value = C.c_void_p()
        try:
            if self.ax.AXUIElementCopyAttributeValue(node, name, C.byref(value)) != 0:
                return None
            return value.value
        finally:
            self.cf.CFRelease(name)

    def label(self, node):
        parts = []
        for key in ('AXTitle', 'AXDescription', 'AXValue'):
            value = self.attr(node, key)
            if value:
                try:
                    parts.append(self.text(value))
                finally:
                    self.cf.CFRelease(value)
        return ' '.join(part for part in parts if part)

    def buttons(self, pid, title):
        app = self.ax.AXUIElementCreateApplication(pid)
        found = []

        def visit(node, depth):
            if depth > 18:
                return
            role = self.attr(node, 'AXRole')
            try:
                if role and self.text(role) == 'AXButton' and title in self.label(node):
                    found.append(self.cf.CFRetain(node))
            finally:
                if role:
                    self.cf.CFRelease(role)
            children = self.attr(node, 'AXChildren')
            if children:
                try:
                    for index in range(self.cf.CFArrayGetCount(children)):
                        visit(self.cf.CFArrayGetValueAtIndex(children, index), depth + 1)
                finally:
                    self.cf.CFRelease(children)

        try:
            visit(app, 0)
        finally:
            self.cf.CFRelease(app)
        return found

    def press(self, node):
        action = self.string('AXPress')
        try:
            assert self.ax.AXUIElementPerformAction(node, action) == 0, 'button press failed'
        finally:
            self.cf.CFRelease(action)

    def activate(self, pid):
        app = self.msg_int(
            self.cls(b'NSRunningApplication'),
            self.sel(b'runningApplicationWithProcessIdentifier:'),
            pid,
        )
        assert app, 'host application was not found'
        assert self.msg_ulong(app, self.sel(b'activateWithOptions:'), 1 << 1), 'host did not activate'

    def key(self, pid, code):
        for down in (True, False):
            event = self.ax.CGEventCreateKeyboardEvent(None, code, down)
            assert event, 'keyboard event was not created'
            try:
                self.ax.CGEventPostToPid(pid, event)
            finally:
                self.cf.CFRelease(event)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--bins', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--timeout', type=int, default=180)
    args = parser.parse_args()
    bins, out = args.bins.resolve(), args.out.resolve()
    out.mkdir(parents=True, exist_ok=False)
    for name in ('home', 'config', 'data', 'state', 'project', 'frames'):
        (out / name).mkdir()
    wrapper = out / 'home' / 'sh'
    wrapper.write_text('#!/bin/sh\nexec /bin/sh "$@"\n')
    wrapper.chmod(0o755)
    runtime = Path(tempfile.mkdtemp(prefix='pconfirm-'))
    config = out / 'config.toml'
    config.write_text(
        'splash = false\nstart_at_login = false\nautomatic_update_checks = true\n'
        'install_agent_skills = false\nchrome_style = "graphite"\n'
    )
    env = {k: v for k, v in os.environ.items() if not k.startswith(('PMUX', 'PRISMATTYC_', 'XDG_'))}
    socket_path = runtime / 'pmux.sock'
    env.update(
        HOME=str(out / 'home'),
        SHELL=str(wrapper),
        PATH=f'{bins}:/usr/bin:/bin:/usr/sbin:/sbin',
        PMUX=str(bins / 'pmux'),
        PMUX_SOCKET=str(socket_path),
        XDG_RUNTIME_DIR=str(runtime),
        XDG_CONFIG_HOME=str(out / 'config'),
        XDG_DATA_HOME=str(out / 'data'),
        XDG_STATE_HOME=str(out / 'state'),
        PRISMATTYC_CONFIG=str(config),
        PRISMATTYC_NO_AGENT_SKILLS='1',
        PRISMATTYC_UI_EVENTS='1',
        PRISMATTYC_DUMP_PRESENT=str(out / 'frames' / 'frame-{seq}.png'),
    )
    result = {'status': 'FAIL', 'platform': sys.platform, 'checks': []}
    owned = []
    desktop = Desktop()

    def cli(*words, timeout=60):
        completed = subprocess.run(
            [str(bins / 'pmux'), '--socket', str(socket_path), *words],
            env=env, cwd=out / 'project', capture_output=True, text=True, timeout=timeout,
        )
        if completed.returncode:
            raise AssertionError(
                f'pmux {" ".join(words)} exited {completed.returncode}: {completed.stdout}{completed.stderr}'
            )
        return completed.stdout

    def wait(probe, label, timeout=None):
        deadline = time.monotonic() + (args.timeout if timeout is None else timeout)
        while time.monotonic() < deadline:
            try:
                value = probe()
                if value:
                    return value
            except (OSError, KeyError, IndexError, ValueError, TypeError, AssertionError):
                pass
            time.sleep(0.1)
        raise AssertionError('timeout: ' + label)

    def window():
        return json.loads(cli('render-status', '--json'))['windows'][0]

    def modal():
        return window().get('restart_modal')

    def record(check):
        result['checks'].append(check)
        print('PASS ' + check, flush=True)

    def capture(name, state):
        seq = state.get('seq')
        source = out / 'frames' / f'frame-{seq:06}.png' if isinstance(seq, int) else None
        deadline = time.monotonic() + 5
        while source and not source.exists() and time.monotonic() < deadline:
            time.sleep(0.05)
        if source and source.exists():
            shutil.copy2(source, out / f'{name}.png')
        else:
            frames = sorted((out / 'frames').glob('frame-*.png'))
            assert frames, 'no presented frame was written'
            shutil.copy2(frames[-1], out / f'{name}.png')
        (out / f'{name}.json').write_text(json.dumps(state, indent=2))

    try:
        with (out / 'daemon.log').open('w') as log:
            owned.append(subprocess.Popen(
                [str(bins / 'pmuxd'), '--socket', str(socket_path)],
                env=env, cwd=out / 'project', stdout=log, stderr=log,
            ))
        wait(lambda: json.loads(cli('versions'))['daemon'].get('pid'), 'private daemon', 60)
        for name in ('alpha', 'beta'):
            cli('new', '--no-attach', name)
        host = subprocess.Popen(
            [str(bins / 'prismattyc-host'), '--no-splash', '--attach-session', 'alpha'],
            env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, errors='replace',
        )
        owned.append(host)
        host_log = (out / 'host.log').open('w')

        def read_host():
            for line in host.stdout:
                host_log.write(line)
                host_log.flush()

        threading.Thread(target=read_host, daemon=True).start()
        wait(lambda: window()['last_raster']['present_succeeded'], 'first frame', 60)
        record('private host presented a frame')
        desktop.activate(host.pid)

        def review_button():
            found = desktop.buttons(host.pid, 'Review Update')
            return found[0] if len(found) == 1 else None

        button = wait(review_button, 'Review Update alert', args.timeout)
        desktop.press(button)
        desktop.cf.CFRelease(button)
        record('Review Update was pressed on the update alert')

        def ready():
            current = modal()
            if not current or current.get('title') != 'Restart the pmux server':
                return None
            if not current.get('expanded') or current.get('sessions_shown', 0) < 2:
                return None
            rows = current.get('rows') or []
            if not any(row.startswith('Save and restart:') for row in rows):
                return None
            if not any(row.startswith('Restart when idle:') for row in rows):
                return None
            if any('Restart without saving' in row for row in rows):
                return None
            if not any(row == 'alpha' for row in rows) or not any(row == 'beta' for row in rows):
                return None
            status = json.loads(cli('render-status', '--json'))
            current['seq'] = status['windows'][0]['last_raster'].get('capture_seq')
            return current

        shown = wait(ready, 'one-confirm dialog')
        capture('sessions', shown)
        result['dialog'] = {
            'title': shown['title'],
            'version': shown.get('version'),
            'selected': shown.get('selected'),
            'sessions_shown': shown.get('sessions_shown'),
        }
        record('one dialog lists the sessions and Save and restart')
        desktop.key(host.pid, 125)  # Down

        def idle_selected():
            current = modal()
            rows = (current or {}).get('rows') or []
            selected = (current or {}).get('selected')
            if selected is None or selected >= len(rows):
                return None
            if not rows[selected].startswith('Restart when idle:'):
                return None
            status = json.loads(cli('render-status', '--json'))
            current['seq'] = status['windows'][0]['last_raster'].get('capture_seq')
            return current

        chosen = wait(idle_selected, 'Restart when idle selected', 30)
        capture('idle-choice', chosen)
        record('Restart when idle is the other choice')
        desktop.key(host.pid, 36)  # Return

        def waiting():
            details = window().get('space_details') or {}
            rows = details.get('rows') or []
            if modal() is not None:
                return None
            if 'will restart after the last session ends' not in '\n'.join(rows):
                return None
            return rows

        message = wait(waiting, 'restart when idle message', 30)
        status = json.loads(cli('render-status', '--json'))
        capture('idle-waiting', {'rows': message, 'seq': status['windows'][0]['last_raster'].get('capture_seq')})
        running = cli('ls')
        assert 'alpha' in running.split() and 'beta' in running.split(), running
        marker = list((out / 'data').rglob('restart-when-idle'))
        assert len(marker) == 1, marker
        body = marker[0].read_bytes()
        assert body.startswith(b'v1\nrelease='), body
        assert str(marker[0]).startswith(str(out / 'data')), marker[0]
        result['sessions_still_running'] = running.split()
        record('Restart when idle leaves both sessions running')
        result['status'] = 'PASS'
    except BaseException as error:
        result['error'] = f'{type(error).__name__}: {error}'
        try:
            result['last_window'] = window()
        except Exception as detail:
            result['last_window'] = f'unavailable: {detail}'
        raise
    finally:
        for lookup in (
            lambda: json.loads(cli('render-status', '--json')).get('host_pid'),
            lambda: json.loads(cli('versions'))['daemon'].get('pid'),
        ):
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
    print(json.dumps({'status': result['status'], 'checks': result['checks']}))
    return 0


if __name__ == '__main__':
    sys.exit(main())
