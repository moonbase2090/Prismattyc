#!/usr/bin/env python3
"""The pmux server card: a failed start, Try again, a stop, Start, local shell.

Drives a real Prismattyc host against a private pmux server. Keys go to the
host process (macOS) or its X11 window (Linux, under Xvfb). The host's own
presented frames are saved for each step, with its render status.

Usage: server-card-e2e.py --bins DIR --out DIR
"""
import argparse
import ctypes
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

MAC = sys.platform == 'darwin'
MAC_CODES = {'t': 17, 'u': 32, 'Return': 36, 'Down': 125}
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
    runtime = Path(tempfile.mkdtemp(prefix='pcard-', dir='/tmp'))
    for name in ('home', 'config', 'data', 'state', 'project'):
        (out / name).mkdir()
    # Login shells take their prompt from the system profile, which names the
    # machine; frames are published, so every shell gets a neutral prompt.
    for profile in ('.profile', '.bash_profile', '.bashrc', '.zshrc'):
        (out / 'home' / profile).write_text("PS1='card> '\nPROMPT='card> '\n")
    config = out / 'config.toml'
    config.write_text('splash = false\nstart_at_login = false\nautomatic_update_checks = false\n'
                      'install_agent_skills = false\nspace_startup = "fresh"\nchrome_style = "graphite"\n'
                      '[keys]\nupdate_restart = "ctrl+alt+u"\n')
    # `pmux up` fails while the mode file says so; everything else is real.
    mode = out / 'mode'
    mode.write_text('fail')
    wrapper = out / 'pmux-wrapper'
    wrapper.write_text(f'''#!/bin/sh
if [ "$(cat {mode})" = fail ]; then
  for word in "$@"; do
    if [ "$word" = up ]; then
      echo "pmuxd: socket directory is not writable" >&2
      exit 1
    fi
  done
fi
exec {bins / 'pmux'} "$@"
''')
    wrapper.chmod(0o755)
    env = {k: v for k, v in os.environ.items() if not k.startswith(('PMUX', 'PRISMATTYC_', 'XDG_'))}
    env.update(HOME=str(out / 'home'), SHELL='/bin/sh', PS1='card> ',
               PATH=f'{bins}:/usr/bin:/bin:/usr/sbin:/sbin', PMUX=str(wrapper),
               PMUX_SOCKET=str(runtime / 'pmux.sock'), XDG_RUNTIME_DIR=str(runtime),
               XDG_DATA_HOME=str(out / 'data'), XDG_CONFIG_HOME=str(out / 'config'),
               XDG_STATE_HOME=str(out / 'state'), PRISMATTYC_CONFIG=str(config),
               PRISMATTYC_NO_AGENT_SKILLS='1', PRISMATTYC_DUMP_PRESENT=str(out / 'present.png'),
               PRISMATTYC_E2E_WINDOW_CELLS='120x34')
    result = {'status': 'FAIL', 'platform': sys.platform, 'checks': []}
    host = None

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
            time.sleep(.2)
        raise AssertionError('timeout: ' + label)

    # Render status is a file the host writes beside the socket, so it
    # answers while the server is down.
    def window():
        return json.loads(cli('render-status', '--json'))['windows'][0]

    if MAC:
        cg = ctypes.cdll.LoadLibrary('/System/Library/Frameworks/ApplicationServices.framework/ApplicationServices')
        cf = ctypes.cdll.LoadLibrary('/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation')
        cg.CGEventCreateKeyboardEvent.restype = ctypes.c_void_p
        cg.CGEventCreateKeyboardEvent.argtypes = [ctypes.c_void_p, ctypes.c_uint16, ctypes.c_bool]
        cg.CGEventSetFlags.argtypes = [ctypes.c_void_p, ctypes.c_uint64]
        cg.CGEventPostToPid.argtypes = [ctypes.c_int, ctypes.c_void_p]
        cf.CFRelease.argtypes = [ctypes.c_void_p]

    def seq():
        return status()['last_raster']['capture_seq']

    def key(name, mods=()):
        try:
            before = seq()
        except (AssertionError, KeyError, ValueError):
            before = None
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
            subprocess.run(['xdotool', 'key', '--clearmodifiers', '+'.join([*mods, name])],
                           check=True, env=env)
        # Render status is published with each presented frame.
        if before is not None:
            try:
                wait(lambda: seq() > before, 'frame after ' + name, timeout=5)
            except AssertionError:
                pass
        time.sleep(.3)

    def capture(name):
        time.sleep(1.2)
        shutil.copy(out / 'present.png', frames / f'{name}.png')

    def record(check):
        result['checks'].append(check)
        print('PASS ' + check, flush=True)

    status = window

    def select_row(prefix):
        """Arrow to the update-menu row starting with prefix, one step at a time."""
        details = wait(lambda: (d := status().get('space_details')) and any(
            row.startswith(prefix) for row in d['rows']) and d, 'menu row ' + prefix)
        target = next(i for i, row in enumerate(details['rows']) if row.startswith(prefix))
        for _ in range(len(details['rows']) + 2):
            current = status()['space_details']['selected']
            if current == target:
                return
            key('Down')
            wait(lambda: status()['space_details']['selected'] != current, 'selection moved')
        raise AssertionError(f'never reached {prefix}: {status()["space_details"]}')

    try:
        host = subprocess.Popen([str(bins / 'prismattyc-host')], env=env, cwd=out / 'project',
                                stdout=open(out / 'host.log', 'w'), stderr=subprocess.STDOUT,
                                start_new_session=True)
        wait(lambda: (out / 'present.png').exists(), 'first frame', timeout=60)
        assert host.poll() is None, 'the app quit after a failed start'
        if not MAC:
            wid = wait(lambda: subprocess.run(['xdotool', 'search', '--onlyvisible', '--pid', str(host.pid)],
                                              capture_output=True, text=True, env=env).stdout.split()[-1],
                       'host window')
            subprocess.run(['xdotool', 'windowactivate', '--sync', wid], env=env)
            subprocess.run(['xdotool', 'windowfocus', '--sync', wid], env=env)
        state = wait(lambda: (s := status()) and s.get('server_card') and s, 'card after a failed start')
        assert state['server'].startswith('failed:') and 'not writable' in state['server'], state['server']
        capture('1-failed-start')
        record('failed start: the window opened with the card and the reason')

        mode.write_text('ok')
        key('Return')
        state = wait(lambda: (s := status()) and s['server'] == 'running' and not s['server_card'] and s,
                     'Try again starts the server', timeout=60)
        capture('2-started')
        record('try again: Enter started the server and the card went away')

        cli('space', 'create', 'work', '--session-name', 'work-1', '--no-attach')
        cli('space', 'open', 'work', '--no-run')
        wait(lambda: status()['space'] == 'work' and status()['focused_session'], 'work open')
        # The window has only the Space's session now: stop the server.
        cli('stop')
        state = wait(lambda: (s := status()) and s['server_card'] and s, 'card after a stop', timeout=30)
        assert state['server'] == 'stopped', state['server']
        assert host.poll() is None
        capture('3-stopped')
        record('stop: one card replaced the attached pane after the grace')

        key('Return')
        wait(lambda: (s := status()) and s['server'] == 'running' and not s['server_card'], 'Start', timeout=60)
        wait(lambda: status()['space'] == 'work', 'Space still current')
        capture('4-restarted')
        record('start: Enter started the server and the card went away')

        cli('stop')
        wait(lambda: status()['server_card'], 'card after the second stop', timeout=30)
        tabs = status()['tab_count']
        key('t', ('ctrl', 'shift'))
        try:
            wait(lambda: (s := status()) and not s['server_card'] and s['tab_count'] == tabs + 1,
                 'local shell', timeout=15)
        except AssertionError:
            s = status()
            raise AssertionError(f"local shell: card={s['server_card']} tabs={s['tab_count']} "
                                 f"before={tabs} server={s['server']}") from None
        assert status()['server'] == 'stopped'
        capture('5-local-shell')
        record('local shell: the new-tab key opened a local shell and the card stepped aside')

        # Stop from Update and restart: it asks first, saves the sessions,
        # clears a pending restart-when-idle, and Start brings them back.
        cli('up')
        wait(lambda: status()['server'] == 'running', 'running again', timeout=60)
        # The plain stop above ended work-1; this session is the one to save.
        cli('new', '--no-attach', 'kept', '--', '/bin/sh')
        work_file = next(path for root in (out, runtime) for path in Path(root).rglob('work.json'))
        marker = work_file.parent / 'restart-when-idle'
        marker.write_text('v1\nrelease=0.0.0\n')
        key('u', ('ctrl', 'alt'))
        select_row('Stop the pmux server')
        key('Return')
        details = wait(lambda: (d := status().get('space_details')) and any(
            row.startswith('Enter again saves') for row in d['rows']) and d, 'stop asks first')
        assert status()['server'] == 'running', 'nothing stops on the first Enter'
        capture('6-stop-confirm')
        key('Return')
        wait(lambda: status()['server'] == 'stopped', 'Save and stop', timeout=60)
        assert not marker.exists(), 'the restart-when-idle marker is cleared'
        # This window still has the local shell from above, so no card.
        assert not status()['server_card']
        record('stop: the update menu asked first, then saved, stopped and cleared restart-when-idle')
        key('u', ('ctrl', 'alt'))
        select_row('Start the pmux server')
        capture('7-start-row')
        key('Return')
        wait(lambda: status()['server'] == 'running' and not status()['server_card'], 'Start', timeout=60)
        assert 'kept' in cli('ls'), 'the saved session came back'
        capture('8-restored')
        record('start: the saved session came back')
        result['status'] = 'PASS'
    finally:
        (out / 'result.json').write_text(json.dumps(result, indent=2))
        if host and host.poll() is None:
            host.terminate()
            try:
                host.wait(10)
            except subprocess.TimeoutExpired:
                host.kill()
        subprocess.run([str(bins / 'pmux'), '--socket', env['PMUX_SOCKET'], 'stop'], env=env,
                       capture_output=True, timeout=30)
        shutil.rmtree(runtime, ignore_errors=True)
    print('SERVER_CARD_E2E_COMPLETE: ' + result['status'], flush=True)


if __name__ == '__main__':
    main()
