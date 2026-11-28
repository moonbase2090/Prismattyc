#!/usr/bin/env python3
"""Attach running session: open, pick, focus existing, detach, reopen, move.

Drives a real Prismattyc host against a private pmux server. Keys go to the
host process (macOS) or its X11 window (Linux, under Xvfb). The host's own
presented frames are saved for each step, with its render status.

Usage: attach-session-e2e.py --bins DIR --out DIR
"""
import argparse
import ctypes
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

MAC = sys.platform == 'darwin'
ATTACH = ('a', ('ctrl', 'alt'))
MAC_CODES = {'a': 0, 'x': 7, '1': 18, 'Return': 36, 'Escape': 53, 'Down': 125, 'Up': 126}
MAC_FLAGS = {'shift': 1 << 17, 'ctrl': 1 << 18, 'alt': 1 << 19}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bins', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    args = parser.parse_args()
    bins, out = args.bins.resolve(), args.out.resolve()
    out.mkdir(parents=True, exist_ok=False)
    frames = out / 'frames'
    frames.mkdir()
    runtime = Path(tempfile.mkdtemp(prefix='pattach-', dir='/tmp'))
    for name in ('home', 'config', 'data', 'state', 'project'):
        (out / name).mkdir()
    config = out / 'config.toml'
    config.write_text('splash = false\nstart_at_login = false\nautomatic_update_checks = false\n'
                      'install_agent_skills = false\nspace_startup = "fresh"\nchrome_style = "graphite"\n'
                      '[keys]\nattach_session = "ctrl+alt+a"\n')
    env = {k: v for k, v in os.environ.items() if not k.startswith(('PMUX', 'PRISMATTYC_', 'XDG_'))}
    env.update(HOME=str(out / 'home'), SHELL='/bin/sh', PS1='attach> ',
               PATH=f'{bins}:/usr/bin:/bin:/usr/sbin:/sbin', PMUX=str(bins / 'pmux'),
               PMUX_SOCKET=str(runtime / 'pmux.sock'), XDG_RUNTIME_DIR=str(runtime),
               XDG_DATA_HOME=str(out / 'data'), XDG_CONFIG_HOME=str(out / 'config'),
               XDG_STATE_HOME=str(out / 'state'), PRISMATTYC_CONFIG=str(config),
               PRISMATTYC_NO_AGENT_SKILLS='1', PRISMATTYC_DUMP_PRESENT=str(out / 'present.png'),
               PRISMATTYC_E2E_WINDOW_CELLS='120x34')
    result = {'status': 'FAIL', 'platform': sys.platform, 'checks': []}
    owned = []

    def cli(*words, check=True):
        p = subprocess.run([str(bins / 'pmux'), '--socket', env['PMUX_SOCKET'], *words], env=env,
                           cwd=out / 'project', capture_output=True, text=True, timeout=120)
        with (out / 'commands.jsonl').open('a') as log:
            log.write(json.dumps({'args': list(words), 'exit': p.returncode,
                                  'stdout': p.stdout[-4000:], 'stderr': p.stderr[-4000:]}) + '\n')
        if check and p.returncode:
            raise AssertionError(f'pmux {" ".join(words)} failed: {p.stderr.strip()}')
        return p.stdout

    def wait(probe, label, timeout=30):
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            try:
                value = probe()
                if value:
                    return value
            except (OSError, KeyError, ValueError, TypeError, IndexError, AssertionError):
                pass
            time.sleep(.15)
        raise AssertionError('timeout: ' + label)

    def snapshot():
        with socket.socket(socket.AF_UNIX) as stream:
            stream.settimeout(5)
            stream.connect(env['PMUX_SOCKET'])
            stream.sendall(b'{"version":1,"request_id":1,"type":"snapshot"}\n')
            return json.loads(stream.makefile().readline())['response']['snapshot']

    def session(name):
        return next(s for s in snapshot()['sessions'] if s['name'] == name)

    def space_id(name):
        # Each Space was created with one session of its own.
        return session({'work': 'work-1', 'release': 'rel-1'}[name]).get('space_id')

    def window():
        return json.loads(cli('render-status', '--json'))['windows'][0]

    def seq():
        return window()['last_raster']['capture_seq']

    if MAC:
        cg = ctypes.cdll.LoadLibrary('/System/Library/Frameworks/ApplicationServices.framework/ApplicationServices')
        cf = ctypes.cdll.LoadLibrary('/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation')
        cg.CGEventCreateKeyboardEvent.restype = ctypes.c_void_p
        cg.CGEventCreateKeyboardEvent.argtypes = [ctypes.c_void_p, ctypes.c_uint16, ctypes.c_bool]
        cg.CGEventSetFlags.argtypes = [ctypes.c_void_p, ctypes.c_uint64]
        cg.CGEventPostToPid.argtypes = [ctypes.c_int, ctypes.c_void_p]
        cf.CFRelease.argtypes = [ctypes.c_void_p]

    def key(name, mods=()):
        before = seq()
        if MAC:
            flags = 0
            for mod in mods:
                flags |= MAC_FLAGS[mod]
            for pressed in (True, False):
                event = cg.CGEventCreateKeyboardEvent(None, MAC_CODES[name], pressed)
                cg.CGEventSetFlags(event, flags)
                cg.CGEventPostToPid(host.pid, event)
                cf.CFRelease(event)
        else:
            combo = '+'.join([*mods, name])
            subprocess.run(['xdotool', 'key', '--clearmodifiers', combo], check=True, env=env)
        # Render status is published with each presented frame.
        try:
            wait(lambda: seq() > before, 'frame after ' + name, timeout=5)
        except AssertionError:
            pass
        time.sleep(.3)

    def capture(name):
        time.sleep(1)
        shutil.copy(out / 'present.png', frames / f'{name}.png')
        (frames / f'{name}.json').write_text(json.dumps(window(), indent=2))

    def record(check):
        result['checks'].append(check)
        print('PASS ' + check, flush=True)

    def picker():
        return window().get('attach_picker')

    def open_picker():
        key(*ATTACH)
        return wait(picker, 'attach picker opens')

    def row(state, name):
        return next(r for r in state['rows'] if r['name'] == name)

    def choose(name, mods=()):
        state = wait(picker, 'picker')
        for _ in range(len(state['rows']) + 1):
            if picker()['selected'] == name:
                break
            key('Down')
        assert picker()['selected'] == name, f'could not select {name}: {picker()}'
        key('Return', mods)

    def focused_session():
        return window().get('focused_session')

    try:
        daemon = subprocess.Popen([str(bins / 'pmuxd'), '--socket', env['PMUX_SOCKET'], '--', '/bin/sh'],
                                  env=env, cwd=out / 'project', stdout=open(out / 'daemon.log', 'w'),
                                  stderr=subprocess.STDOUT, start_new_session=True)
        owned.append(daemon)
        wait(lambda: Path(env['PMUX_SOCKET']).is_socket(), 'private pmux server')
        wait(lambda: snapshot() is not None, 'server answers')
        cli('space', 'create', 'work', '--session-name', 'work-1', '--no-attach')
        cli('space', 'create', 'release', '--session-name', 'rel-1', '--no-attach')
        cli('new', '--no-attach', 'loose', '--', '/bin/sh')
        cli('new', '--no-attach', 'build', '--', '/bin/sh')
        cli('space', 'add', 'release', '--session', 'build')
        work = space_id('work')
        assert session('loose').get('space_id') is None
        assert session('build').get('space_id') == space_id('release')

        host = subprocess.Popen([str(bins / 'prismattyc-host')], env=env, cwd=out / 'project',
                                stdout=open(out / 'host.log', 'w'), stderr=subprocess.STDOUT,
                                start_new_session=True)
        owned.append(host)
        wait(lambda: (out / 'present.png').exists() and seq() > 0, 'first frame', timeout=60)
        if not MAC:
            wid = wait(lambda: subprocess.run(['xdotool', 'search', '--onlyvisible', '--pid', str(host.pid)],
                                              capture_output=True, text=True, env=env).stdout.split()[-1],
                       'host window')
            subprocess.run(['xdotool', 'windowactivate', '--sync', wid], env=env)
            subprocess.run(['xdotool', 'windowfocus', '--sync', wid], env=env)
        cli('space', 'open', 'work', '--no-run')
        wait(lambda: window()['space'] == 'work' and focused_session(), 'window shows the work Space')
        work_1 = str(session('work-1')['id'])
        loose = str(session('loose')['id'])
        build = str(session('build')['id'])

        # Open: the picker groups every live session by where it is.
        state = open_picker()
        capture('1-picker')
        assert row(state, 'loose')['group'] == 'NOT IN A SPACE', state
        assert row(state, 'build')['group'] == 'IN OTHER SPACES', state
        assert row(state, 'build')['action'] == 'Move here', state
        assert row(state, 'work-1')['group'] == 'ALREADY OPEN', state
        record('open: picker lists loose, other-Space and open sessions in their groups')

        # Pick: a loose session joins this Space and opens in a new tab.
        tabs = window()['tab_count']
        choose('loose')
        wait(lambda: focused_session() == loose and picker() is None, 'loose opens focused')
        wait(lambda: session('loose').get('space_id') == work, 'loose joins work')
        assert window()['tab_count'] == tabs + 1, window()['tab_count']
        capture('2-picked-loose')
        record('pick: loose joined work and opened in a new focused tab')

        # Focus existing: picking an open session focuses its tab, no new tab.
        key('1', ('ctrl', 'shift'))
        wait(lambda: focused_session() == work_1, 'first tab focused')
        state = open_picker()
        assert row(state, 'loose')['group'] == 'ALREADY OPEN', state
        assert row(state, 'loose')['action'] == 'Focus', state
        tabs = window()['tab_count']
        choose('loose')
        wait(lambda: focused_session() == loose and picker() is None, 'existing tab focused')
        assert window()['tab_count'] == tabs, 'focus must not open another tab'
        record('focus existing: picking an open session focused its tab')

        # Detach: the tab closes and the session keeps running in work.
        key('x', ('ctrl', 'shift'))
        wait(lambda: window()['tab_count'] == tabs - 1 and focused_session() != loose, 'tab detached')
        assert session('loose').get('space_id') == work
        capture('3-detached')
        record('detach: the tab closed and loose kept running in work')

        # Reopen: the detached session is in this Space and opens again.
        state = open_picker()
        assert row(state, 'loose')['group'] == 'IN THIS SPACE', state
        choose('loose')
        wait(lambda: focused_session() == loose and picker() is None, 'loose reopened')
        assert window()['tab_count'] == tabs
        capture('4-reopened')
        record('reopen: loose opened again from IN THIS SPACE')

        # Move: a session owned by another Space asks first, naming that Space.
        open_picker()
        choose('build')
        state = wait(lambda: (p := picker()) and p.get('confirm') and p, 'move confirmation')
        assert 'leaves release and joins work' in state['confirm'], state
        assert session('build').get('space_id') == space_id('release'), 'nothing moves before Enter'
        capture('5-confirm-move')
        key('Return')
        wait(lambda: focused_session() == build and picker() is None, 'build opens focused')
        wait(lambda: session('build').get('space_id') == work, 'build moved to work')
        capture('6-moved-build')
        record('move: build moved from release to work after a confirmation naming release')

        # Recorded, not asserted: detaching a Space-owned session makes the
        # next autosave refuse to drop it, and autosave then waits for a
        # manual save. That predates the picker; see the PR's follow-ups.
        result['space_save_status_after_detach'] = window()['space_save_status']

        # Escape closes the picker without changes.
        open_picker()
        key('Escape')
        wait(lambda: picker() is None, 'Escape closes the picker')
        record('escape: the picker closes')
        result['status'] = 'PASS'
    finally:
        (out / 'result.json').write_text(json.dumps(result, indent=2))
        for proc in reversed(owned):
            if proc.poll() is None:
                proc.terminate()
                try:
                    proc.wait(10)
                except subprocess.TimeoutExpired:
                    proc.kill()
        shutil.rmtree(runtime, ignore_errors=True)
    print('ATTACH_SESSION_E2E_COMPLETE: ' + result['status'], flush=True)


if __name__ == '__main__':
    main()
