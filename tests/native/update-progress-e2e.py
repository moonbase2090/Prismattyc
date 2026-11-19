#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Drive Update and restart in a real window and record its progress meter.

Disposable machines only. A full run replaces the installed Prismattyc with the
newest public release, exactly as a user's update would.

Linux and Windows: the binaries in --bins are first copied into OUT/install,
because the updater refuses to replace a build tree, and the host runs from
there. Their version must be older than the newest release. macOS: the updater
replaces the installed Prismattyc.app, which must be older than the newest
release; the host runs from --bins.

--fail-download NAME (Linux) puts a curl wrapper first on PATH that cuts the
download of the release file for component NAME short. The meter must then
hold step 1 with the error instead of moving on.

Keys go to the host only: Quartz events to its PID on macOS, XTEST through
xdotool on its focused window on Linux, SendInput to its foreground window on
Windows. The script records every change of the panel's meter lines from
render-status and copies each presented frame (PRISMATTYC_DUMP_PRESENT).

  update-progress-e2e.py --bins target/release --out DIR [--fail-download pmux-attach]
"""
import argparse
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

STEP = 'Step '
FAILED = 'Failed at step '
BINARIES = ('pmux', 'pmuxd', 'pmux-attach', 'pmux-mcp', 'prismattyc', 'prismattyc-host')
EXE = '.exe' if os.name == 'nt' else ''
BINDING = 'ctrl+alt+u'


class Keys:
    """Native key input for one host process."""

    def __init__(self, pid):
        self.pid = pid
        if sys.platform == 'darwin':
            import ctypes
            import ctypes.util
            self.quartz = ctypes.CDLL(ctypes.util.find_library('ApplicationServices'))
            self.quartz.CGEventCreateKeyboardEvent.restype = ctypes.c_void_p
            self.quartz.CGEventCreateKeyboardEvent.argtypes = [ctypes.c_void_p, ctypes.c_uint16, ctypes.c_bool]
            self.quartz.CGEventSetFlags.argtypes = [ctypes.c_void_p, ctypes.c_uint64]
            self.quartz.CGEventPostToPid.argtypes = [ctypes.c_int, ctypes.c_void_p]
            self.quartz.CFRelease.argtypes = [ctypes.c_void_p]

    # Each name maps to (macOS key code, X keysym, Windows virtual key).
    CODES = {'u': (32, 'u', 0x55), 'Home': (115, 'Home', 0x24), 'Down': (125, 'Down', 0x28),
             'Return': (36, 'Return', 0x0D), 'Escape': (53, 'Escape', 0x1B)}

    def press(self, name, ctrl=False, alt=False):
        mac, keysym, vk = self.CODES[name]
        if sys.platform == 'darwin':
            flags = (1 << 18 if ctrl else 0) | (1 << 19 if alt else 0)
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
            chord = '+'.join([*(['ctrl'] if ctrl else []), *(['alt'] if alt else []), keysym])
            subprocess.check_call(['xdotool', 'key', '--clearmodifiers', chord])
        else:
            self._windows(vk, ctrl, alt)
        time.sleep(.15)

    def _windows(self, vk, ctrl, alt):
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

        keys = [*([0x11] if ctrl else []), *([0x12] if alt else []), vk]
        events = [(k, 0) for k in keys] + [(k, 2) for k in reversed(keys)]
        array = (INPUT * len(events))()
        for slot, (key, flags) in zip(array, events):
            slot.type = 1
            slot.ki = KEYBDINPUT(key, 0, flags, 0, 0)
        sent = user32.SendInput(len(events), array, ctypes.sizeof(INPUT))
        assert sent == len(events), f'SendInput sent {sent} of {len(events)}'


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--bins', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--fail-download', choices=BINARIES)
    parser.add_argument('--timeout', type=int, default=600)
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
    socket_path = runtime / 'pmux.sock'
    env.update(PMUX=str(bins / ('pmux' + EXE)), PMUX_SOCKET=str(socket_path),
               PRISMATTYC_CONFIG=str(config), PRISMATTYC_NO_AGENT_SKILLS='1',
               PRISMATTYC_DUMP_PRESENT=str(out / 'present.png'))
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
        # The named release file arrives slowly and is cut off after three
        # seconds, so the meter shows part of it before the failure.
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
    meter_log = []
    stop = threading.Event()
    host = daemon = None
    owned = []

    def cli(*words, timeout=60):
        p = subprocess.run([str(bins / ('pmux' + EXE)), '--socket', str(socket_path), *words], env=env,
                           cwd=out / 'project', capture_output=True, text=True, timeout=timeout)
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
            except (OSError, KeyError, IndexError, ValueError, AssertionError):
                pass
            time.sleep(.1)
        raise AssertionError('timeout: ' + label)

    def render_status():
        return json.loads(cli('render-status', '--json'))

    def rows():
        panel = render_status()['windows'][0].get('space_details') or {}
        return [row.removesuffix(': ') for row in panel.get('rows', [])]

    def choose(keys, label):
        listing = wait(lambda: (lambda r: r if any(x.startswith(label + ':') or x == label for x in r) else None)(rows()),
                       'row ' + label)
        index = next(i for i, x in enumerate(listing) if x.startswith(label + ':') or x == label)
        keys.press('Home')
        for _ in range(index):
            keys.press('Down')
        keys.press('Return')

    def record(check):
        result['checks'].append(check)
        print('PASS ' + check, flush=True)

    def copy_frames(started):
        sidecar, image = out / 'present.json', out / 'present.png'
        last = None
        with (out / 'frames.jsonl').open('w') as log:
            while not stop.is_set():
                try:
                    before = sidecar.read_bytes()
                    meta = json.loads(before)
                    key = (meta['host_instance'], meta['seq'])
                    if key != last:
                        pixels = image.read_bytes()
                        if sidecar.read_bytes() == before:
                            last = key
                            name = f'{len(list(frames.iterdir())):05d}.png'
                            (frames / name).write_bytes(pixels)
                            log.write(json.dumps({'t': round(time.monotonic() - started, 3), 'file': name,
                                                  **meta}) + '\n')
                            log.flush()
                except (OSError, ValueError, KeyError):
                    pass
                time.sleep(.03)

    def watch_meter(started):
        previous = None
        while not stop.is_set():
            try:
                lines = rows()
            except (OSError, ValueError, KeyError, IndexError, AssertionError):
                lines = None
            if lines is not None:
                head = lines[:3] if lines and lines[0].startswith((STEP, FAILED)) else []
                tail = [x for x in lines[3:] if x] if head else lines
                state = (tuple(head), tuple(tail[:3]))
                if state != previous:
                    previous = state
                    meter_log.append({'t': round(time.monotonic() - started, 3), 'meter': head, 'text': tail[:6]})
            time.sleep(.1)

    try:
        result['update_check'] = json.loads(subprocess.run(
            [str(bins / ('pmux' + EXE)), 'update', '--check', '--json'], env=env, capture_output=True,
            text=True, timeout=120).stdout or '{}')
        assert result['update_check'].get('update_available'), result['update_check']
        with (out / 'daemon.log').open('w') as log:
            daemon = subprocess.Popen([str(bins / ('pmuxd' + EXE)), '--socket', str(socket_path)], env=env,
                                      cwd=out / 'project', stdout=log, stderr=log)
        owned.append(daemon)
        wait(lambda: socket_path.exists() and cli('ls') is not None, 'private daemon')
        cli('new', '--no-attach', 'work')
        started = time.monotonic()
        threading.Thread(target=copy_frames, args=(started,), daemon=True).start()
        host = subprocess.Popen([str(bins / ('prismattyc-host' + EXE)), '--no-splash', '--attach-session', 'work'],
                                env=env, stdout=(out / 'host.log').open('w'), stderr=subprocess.STDOUT)
        owned.append(host)
        wait(lambda: render_status()['windows'][0]['last_raster']['present_succeeded'], 'first frame')
        first_instance = render_status()['host_instance']
        keys = Keys(host.pid)
        keys.press('u', ctrl=True, alt=True)
        choose(keys, 'Update and restart all components')
        wait(lambda: any(x.startswith('Continue without saving') for x in rows()), 'session consent', timeout=120)
        threading.Thread(target=watch_meter, args=(started,), daemon=True).start()
        choose(keys, 'Continue without saving')
        if args.fail_download:
            final = wait(lambda: (lambda r: r if r and r[0].startswith(FAILED) else None)(rows()),
                         'meter holds the failed step', timeout=args.timeout)
            result['final_rows'] = final
            assert final[0] == 'Failed at step 1 of 4: Download', final
            assert final[1].startswith(f'Downloading {args.fail_download}: '), final
            assert f'download failed for {args.fail_download}' in ' '.join(final), final
            assert 'Check again' in final, final
            record('meter held step 1 of 4 with the download error and offered Check again')
        else:
            wait(lambda: render_status()['host_instance'] != first_instance, 'relaunched app', timeout=args.timeout)
            result['after'] = json.loads(cli('versions'))
            record('update finished and the app relaunched')
        time.sleep(1)
    except BaseException as error:
        result['error'] = f'{type(error).__name__}: {error}'
        raise
    finally:
        stop.set()
        time.sleep(.3)
        (out / 'meter.json').write_text(json.dumps(meter_log, indent=2))
        # The app logs each step change (never byte counts); render-status
        # polling alone can miss a step that takes a fraction of a second.
        marker = 'prismattyc-host: update progress: '
        log_text = (out / 'host.log').read_text(errors='replace') if (out / 'host.log').exists() else ''
        steps = [line.split(marker, 1)[1] for line in log_text.splitlines() if marker in line]
        result['step_log'] = steps
        stages = []
        for line in steps:
            stage = line.split(' | ', 1)[0].split(': ', 1)[1]
            if not stages or stages[-1] != stage:
                stages.append(stage)
        result['stages'] = stages
        downloads = [e['meter'][1] for e in meter_log if e['meter'] and e['meter'][0].endswith(': Download')]
        result['download_lines'] = len(downloads)
        result['download_first_last'] = downloads[:1] + downloads[-1:]
        if 'error' not in result:
            expected = ['Download'] if args.fail_download else ['Download', 'Verify', 'Install', 'Restart']
            failed = [x for x in steps if x.startswith(FAILED)]
            if stages == expected and len(downloads) >= 2 and bool(failed) == bool(args.fail_download):
                result['status'] = 'PASS'
                record('stages in order: ' + ' > '.join(stages))
        for process in owned:
            if process and process.poll() is None:
                process.terminate()
        try:
            for pid in {render_status()['host_pid'], json.loads(cli('versions'))['daemon']['pid']}:
                if pid:
                    os.kill(pid, signal.SIGTERM)
        except Exception:
            pass
        for process in owned:
            try:
                process.wait(10)
            except Exception:
                process.kill()
        (out / 'result.json').write_text(json.dumps(result, indent=2))
        shutil.rmtree(runtime, ignore_errors=True)
    print(json.dumps({k: result[k] for k in ('status', 'stages', 'download_first_last')}))
    return 0 if result['status'] == 'PASS' else 1


if __name__ == '__main__':
    sys.exit(main())
