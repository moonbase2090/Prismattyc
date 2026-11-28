#!/usr/bin/env python3
"""Launch splash and a failed server start: the splash owns input, the card waits.

Drives a real app (macOS key events to the app's process) with the splash on
and a pmux wrapper whose `up` always fails and counts each attempt.
Usage: splash-server-card-e2e.py BINS OUT  (macOS)
"""
import ctypes, json, os, shutil, subprocess, sys, tempfile, time
from pathlib import Path
bins, out = Path(sys.argv[1]).resolve(), Path(sys.argv[2]).resolve()
out.mkdir(parents=True, exist_ok=False)
frames = out / 'frames'; frames.mkdir()
runtime = Path(tempfile.mkdtemp(prefix='psc-', dir='/tmp'))
for n in ('home', 'config', 'data', 'state', 'project'): (out / n).mkdir()
for p in ('.profile', '.bash_profile', '.bashrc', '.zshrc'):
    (out / 'home' / p).write_text("PS1='card> '\nPROMPT='card> '\n")
config = out / 'config.toml'
config.write_text('splash = true\nstart_at_login = false\nautomatic_update_checks = false\n'
                  'install_agent_skills = false\nspace_startup = "fresh"\nchrome_style = "graphite"\n')
attempts = out / 'up-attempts'
wrapper = out / 'pmux-wrapper'
wrapper.write_text(f'#!/bin/sh\nfor w in "$@"; do if [ "$w" = up ]; then echo up >> {attempts}; '
                   f'echo "pmuxd: socket directory is not writable" >&2; exit 1; fi; done\nexec {bins / "pmux"} "$@"\n')
wrapper.chmod(0o755)
env = {k: v for k, v in os.environ.items() if not k.startswith(('PMUX', 'PRISMATTYC_', 'XDG_'))}
env.update(HOME=str(out / 'home'), SHELL='/bin/sh', PATH=f'{bins}:/usr/bin:/bin:/usr/sbin:/sbin',
           PMUX=str(wrapper), PMUX_SOCKET=str(runtime / 'pmux.sock'), XDG_RUNTIME_DIR=str(runtime),
           XDG_DATA_HOME=str(out / 'data'), XDG_CONFIG_HOME=str(out / 'config'), XDG_STATE_HOME=str(out / 'state'),
           PRISMATTYC_CONFIG=str(config), PRISMATTYC_NO_AGENT_SKILLS='1',
           PRISMATTYC_DUMP_PRESENT=str(out / 'present.png'), PRISMATTYC_E2E_WINDOW_CELLS='120x34')
cg = ctypes.cdll.LoadLibrary('/System/Library/Frameworks/ApplicationServices.framework/ApplicationServices')
cf = ctypes.cdll.LoadLibrary('/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation')
cg.CGEventCreateKeyboardEvent.restype = ctypes.c_void_p
cg.CGEventCreateKeyboardEvent.argtypes = [ctypes.c_void_p, ctypes.c_uint16, ctypes.c_bool]
cg.CGEventPostToPid.argtypes = [ctypes.c_int, ctypes.c_void_p]
cf.CFRelease.argtypes = [ctypes.c_void_p]
CODES = {'Return': 36, 'Escape': 53}
checks = []

def status():
    p = subprocess.run([str(bins / 'pmux'), '--socket', env['PMUX_SOCKET'], 'render-status', '--json'],
                       env=env, capture_output=True, text=True, timeout=30)
    return json.loads(p.stdout)['windows'][0]

def wait(f, label, t=60):
    end = time.monotonic() + t
    while time.monotonic() < end:
        try:
            v = f()
            if v: return v
        except (OSError, KeyError, ValueError, IndexError): pass
        time.sleep(.2)
    raise AssertionError('timeout: ' + label)

def ups():
    return len(attempts.read_text().splitlines()) if attempts.exists() else 0

def key(name):
    for pressed in (True, False):
        e = cg.CGEventCreateKeyboardEvent(None, CODES[name], pressed)
        cg.CGEventPostToPid(host.pid, e); cf.CFRelease(e)
    time.sleep(1.0)

def capture(name):
    time.sleep(1.2); shutil.copy(out / 'present.png', frames / f'{name}.png')

def record(c):
    checks.append(c); print('PASS ' + c, flush=True)

host = subprocess.Popen([str(bins / 'prismattyc-host')], env=env, cwd=out / 'project',
                        stdout=open(out / 'host.log', 'w'), stderr=subprocess.STDOUT, start_new_session=True)
result = 'FAIL'
try:
    wait(lambda: (out / 'present.png').exists(), 'first frame')
    s = wait(lambda: (st := status()) and st['server'].startswith('failed') and st, 'failed start')
    time.sleep(3)  # longer than any card grace; the card must still be hidden
    s = status()
    assert not s['server_card'], 'the card must not show while the splash is up'
    before = ups()
    capture('1-splash-up-failed-start')
    record(f'failed start at launch: the splash shows, the card waits (server={s["server"][:40]}...)')
    key('Return')
    assert ups() == before, 'Enter on the splash must not reach the card (no new start attempt)'
    s = status()
    if not s['server_card']:
        key('Escape')
    s = wait(lambda: (st := status()) and st['server_card'] and st, 'card after the splash')
    capture('2-card-after-splash')
    record('the splash took Enter (no start attempt); the card appeared once the splash was gone')
    key('Return')
    wait(lambda: ups() > before, 'Try again reaches the card', 30)
    record('with the splash gone, Enter reached the card: Try again made a new start attempt')
    result = 'PASS'
finally:
    (out / 'result.json').write_text(json.dumps({'status': result, 'checks': checks}, indent=2))
    host.terminate()
    try: host.wait(10)
    except subprocess.TimeoutExpired: host.kill()
    shutil.rmtree(runtime, ignore_errors=True)
print('SPLASH_CARD_COMPLETE: ' + result, flush=True)
