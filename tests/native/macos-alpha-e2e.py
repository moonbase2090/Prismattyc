#!/usr/bin/env python3
"""Check native macOS alpha rendering and config reload in a private session.

The captures contain the CPU framebuffer before composition. This fixture
does not prove the appearance of the desktop blur.
"""
import argparse
from collections import Counter
import importlib.util
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
import time

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--bins', type=Path, required=True)
parser.add_argument('--out', type=Path, required=True)
args = parser.parse_args()
assert sys.platform == 'darwin', 'Run this fixture in a macOS desktop session'
bins = args.bins.resolve()
out = args.out.resolve()
out.mkdir(parents=True, exist_ok=False)
spec = importlib.util.spec_from_file_location('host_ux', Path(__file__).with_name('host-ux-e2e.py'))
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
runtime = Path(tempfile.mkdtemp(prefix='pmac-alpha-', dir='/tmp'))
env = {k: v for k, v in os.environ.items() if not k.startswith(('PMUX', 'PRISMATTYC_', 'XDG_'))}
(out / 'home').mkdir()
env.update(HOME=str(out / 'home'), PATH=str(bins) + ':/usr/bin:/bin:/usr/sbin:/sbin',
           SHELL='/bin/bash', PMUX_SOCKET=str(runtime / 'pmux.sock'),
           XDG_RUNTIME_DIR=str(runtime), XDG_CONFIG_HOME=str(out / 'config'),
           XDG_DATA_HOME=str(out / 'data'), XDG_STATE_HOME=str(out / 'state'),
           PRISMATTYC_CONFIG=str(out / 'config.toml'),
           PRISMATTYC_DUMP_PRESENT=str(out / 'present.png'))
host = daemon = None
records = []


def configure(opacity, blur):
    (out / 'config.toml').write_text(
        f'window_opacity = {opacity}\nchrome_opacity = {opacity}\n'
        f'window_blur = {str(blur).lower()}\nfont_px = 16.0\n'
        'focus_border_animation = "none"\n'
        '[theme_overrides]\ndefault_bg = "#405060"\ndefault_fg = "#f0d020"\n')


def wait(fn, label, timeout=20):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        if host is not None:
            assert host.poll() is None, (out / 'host.log').read_text()
        try:
            value = fn()
            if value:
                return value
        except (OSError, ValueError, subprocess.CalledProcessError):
            pass
        time.sleep(0.1)
    raise AssertionError('timeout: ' + label)


def cli(*words):
    return subprocess.check_output([str(bins / 'pmux'), *words], env=env, text=True, timeout=20)


def frame():
    before = (out / 'present.json').read_bytes()
    png = (out / 'present.png').read_bytes()
    after = (out / 'present.json').read_bytes()
    if before != after:
        return None
    image = module.PNG(png)
    assert image.bpp == 4, "Expected straight RGBA capture"
    counts = Counter(bytes(row[i:i + 4]) for row in image.rows for i in range(0, len(row), image.bpp))
    return png, json.loads(after), counts


try:
    configure(1.0, False)
    with (out / 'daemon.log').open('w') as log:
        daemon = subprocess.Popen([str(bins / 'pmuxd'), '--socket', env['PMUX_SOCKET'],
                                   '--', '/bin/bash', '--noprofile', '--norc'],
                                  env=env, stdout=log, stderr=log)
    wait(lambda: Path(env['PMUX_SOCKET']).exists(), 'private daemon')
    cli('space', 'create', 'alpha-probe', '--no-attach')
    with (out / 'host.log').open('w') as log:
        host = subprocess.Popen([str(bins / 'prismattyc-host'), '--no-splash',
                                 '--attach-session', 'alpha-probe-1'],
                                env=env, stdout=log, stderr=log)
    wait(frame, 'native window')
    with socket.socket(socket.AF_UNIX) as connection:
        connection.settimeout(5)
        connection.connect(env['PMUX_SOCKET'])
        connection.sendall(b'{"version":1,"request_id":1,"type":"snapshot"}\n')
        panes = json.loads(connection.makefile().readline())['response']['snapshot']
    (out / 'sessions.json').write_text(json.dumps(panes, indent=2))
    # The attached session is private to this fixture.
    session = next(s for s in panes['sessions'] if s['name'] == 'alpha-probe-1')
    pane = str(session['windows'][0]['panes'][0]['id'])
    cli('pane-write', pane, '--text', "printf '\\033[2J\\033[HOPAQUE TEXT \\033[48;2;17;231;149mEXPLICIT BACKGROUND\\033[0m\\n'",
        '--submit', 'enter', '--json')
    previous_seq = 0
    for name, opacity, blur in [('opaque', 1.0, False), ('transparent', 0.5, False),
                                ('blur-on', 0.5, True), ('opaque-blur', 1.0, True),
                                ('blur-off', 0.5, False), ('restored', 1.0, False)]:
        configure(opacity, blur)
        expected = bytes((64, 80, 96, 255 if opacity == 1.0 else 128))

        def ready():
            value = frame()
            if value is None:
                return None
            _, meta, counts = value
            if meta['seq'] <= previous_seq or counts[expected] < 10000:
                return None
            if counts[bytes((17, 231, 149, 255))] < 100 or counts[bytes((240, 208, 32, 255))] < 30:
                return None
            return value

        # Permit the config watcher to apply blur-only changes too.
        time.sleep(1.2)
        png, meta, counts = wait(ready, name)
        previous_seq = meta['seq']
        (out / (name + '.png')).write_bytes(png)
        records.append(dict(phase=name, host_pid=host.pid, seq=previous_seq,
                            ground_pixels=counts[expected],
                            opaque_background_pixels=counts[bytes((17, 231, 149, 255))],
                            opaque_text_pixels=counts[bytes((240, 208, 32, 255))]))
        print('PASS ' + name, flush=True)
        # Confirm that cursor/row changes can use retained straight pixels at
        # each opacity. Config reload itself must still repaint the full frame.
        cli('pane-write', pane, '--text', "printf '.'", '--submit', 'enter', '--json')

        def partial_ready():
            value = ready()
            if value and not value[1]['full']:
                return value
            return None

        png, partial, _ = wait(partial_ready, name + ' partial repaint')
        previous_seq = partial['seq']
        (out / (name + '-partial.png')).write_bytes(png)
        records[-1]['partial_seq'] = previous_seq
        print('PASS ' + name + ' partial repaint', flush=True)
    log = (out / 'host.log').read_text()
    assert 'Core Animation present (premultiplied ARGB)' in log, log
    assert 'window_blur ignored' not in log and 'window_opacity ignored' not in log, log
    (out / 'result.json').write_text(json.dumps(dict(status='PASS', records=records,
        limitation='CPU captures do not verify composited desktop blur'), indent=2))
except Exception as error:
    (out / 'result.json').write_text(json.dumps(dict(status='FAIL', error=repr(error), records=records), indent=2))
    raise
finally:
    for process in (host, daemon):
        if process and process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)
    shutil.rmtree(runtime)
