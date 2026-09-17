#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Assert the native hyperlink cursor on a private X11 display.

Requires Xvfb, xdotool, ffmpeg, and libXfixes. Set PRISMATTYC_HOST to the
binary under test and HYPERLINK_HOVER_OUT to an empty artifact directory.
"""
import ctypes as C
import hashlib
import json
import os
from pathlib import Path
import select
import subprocess
import time


class CursorImage(C.Structure):
    _fields_ = [(n, t) for n, t in (
        ('x', C.c_short), ('y', C.c_short), ('width', C.c_ushort),
        ('height', C.c_ushort), ('xhot', C.c_ushort), ('yhot', C.c_ushort),
        ('serial', C.c_ulong), ('pixels', C.POINTER(C.c_ulong)),
        ('atom', C.c_ulong), ('name', C.c_char_p))]


def main():
    out = Path(os.environ.get('HYPERLINK_HOVER_OUT', 'build/hyperlink-hover')).resolve()
    out.mkdir(parents=True, exist_ok=True)
    runtime = out / 'runtime'
    runtime.mkdir(mode=0o700, exist_ok=True)
    (out / 'config.toml').write_text(
        'splash = false\nfont_px = 20\nwindow_padding_px = 0\n'
        'pane_padding_px = 0\ntab_strip = "always"\nwindow_opacity = 1.0\n')
    # Distinct backgrounds locate rendered cells without assuming font metrics.
    (out / 'guest.py').write_text(r'''
import sys, tty
tty.setraw(sys.stdin.fileno())
def row(n, color, label):
    sys.stdout.write(f'\x1b[{n};1H\x1b[2K\x1b[48;2;{color}m{label}\x1b[0m')
def named(label):
    row(3, '20;60;80', label)
    sys.stdout.flush()
link = '\x1b]8;;https://moonbase2090.com/\x1b\\Moonbase website\x1b]8;;\x1b\\'
sys.stdout.write('\x1b[2J\x1b[H')
row(1, '60;40;20', 'Ordinary terminal text')
named(link)
row(5, '30;70;50', 'https://moonbase2090.com/')
row(7, '70;30;50', '\x1b]8;;javascript:alert(1)\x1b\\https://unsafe.example\x1b]8;;\x1b\\')
row(9, '50;30;70', '\x1b]8;;日本語のリンク\x1b\\https://unicode.example\x1b]8;;\x1b\\')
sys.stdout.flush()
while True:
    key = sys.stdin.read(1)
    if key == 'r': named('Ordinary replacement')
    if key == 'l': named(link)
    if key == 's':
        sys.stdout.write('\x1b[1S')
        sys.stdout.flush()
''')
    env = dict(os.environ)
    env.update(WINIT_UNIX_BACKEND='x11', WINIT_X11_SCALE_FACTOR='1',
               XDG_RUNTIME_DIR=str(runtime), XDG_CONFIG_HOME=str(out / 'config'),
               XDG_DATA_HOME=str(out / 'data'), PMUX_SOCKET=str(runtime / 'mux.sock'),
               PRISMATTYC_CONFIG=str(out / 'config.toml'))
    env.pop('WAYLAND_DISPLAY', None)
    env.pop('PRISMATTYC_PANE_ID', None)
    host = xvfb = display = None
    result = {'status': 'FAIL', 'cases': []}

    def run(*args):
        return subprocess.check_output(args, env=env, timeout=10)

    def wait_for(probe, description):
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            value = probe()
            if value:
                return value
            time.sleep(0.05)
        raise AssertionError(description)

    try:
        read_fd, write_fd = os.pipe()
        with (out / 'xvfb.log').open('w') as log:
            xvfb = subprocess.Popen(['Xvfb', '-displayfd', str(write_fd), '-screen',
                                     '0', '1280x900x24', '-nolisten', 'tcp'],
                                    pass_fds=[write_fd], stdout=log, stderr=log)
        os.close(write_fd)
        try:
            assert select.select([read_fd], [], [], 10)[0], 'Xvfb startup timeout'
            number = os.read(read_fd, 32).decode().strip()
            assert number.isdigit(), 'Xvfb did not return a display'
            env['DISPLAY'] = ':' + number
        finally:
            os.close(read_fd)
        binary = os.environ.get('PRISMATTYC_HOST', 'target/debug/prismattyc-host')
        with (out / 'host.log').open('w') as log:
            host = subprocess.Popen([binary, '--', 'python3', str(out / 'guest.py')],
                                    env=env, stdout=log, stderr=log)

        def window():
            found = subprocess.run(['xdotool', 'search', '--pid', str(host.pid)],
                                   env=env, capture_output=True, text=True, timeout=5)
            return found.stdout.split()[0] if found.stdout.split() else None

        win = wait_for(window, 'host window missing')
        run('xdotool', 'windowfocus', win)
        x11 = C.CDLL('libX11.so.6')
        x11.XOpenDisplay.argtypes = [C.c_char_p]
        x11.XOpenDisplay.restype = C.c_void_p
        x11.XFree.argtypes = [C.c_void_p]
        x11.XCloseDisplay.argtypes = [C.c_void_p]
        display = x11.XOpenDisplay(env['DISPLAY'].encode())
        assert display, 'XOpenDisplay failed'
        fixes = C.CDLL('libXfixes.so.3')
        fixes.XFixesGetCursorImage.argtypes = [C.c_void_p]
        fixes.XFixesGetCursorImage.restype = C.POINTER(CursorImage)

        def cursor():
            pointer = fixes.XFixesGetCursorImage(display)
            assert pointer, 'XFixesGetCursorImage failed'
            try:
                image = pointer.contents
                pixels = b''.join((int(image.pixels[i]) & 0xffffffff).to_bytes(4, 'little')
                                  for i in range(image.width * image.height))
                return (image.width, image.height, image.xhot, image.yhot,
                        hashlib.sha256(pixels).hexdigest())
            finally:
                x11.XFree(pointer)

        def capture(path=None):
            args = ['ffmpeg', '-v', 'error', '-f', 'x11grab', '-draw_mouse',
                    '1' if path else '0', '-video_size', '1280x900',
                    '-i', env['DISPLAY'], '-frames:v', '1']
            return run(*(args + (['-y', str(path)] if path else
                                ['-f', 'rawvideo', '-pix_fmt', 'rgb24', '-'])))

        colors = [(60, 40, 20), (20, 60, 80), (30, 70, 50),
                  (70, 30, 50), (50, 30, 70)]

        def points():
            pixels = capture()
            locations = []
            for color in colors:
                offset = pixels.find(bytes(color))
                while offset >= 0 and offset % 3:
                    offset = pixels.find(bytes(color), offset + 1)
                if offset < 0:
                    return None
                locations.append((offset // 3 % 1280 + 2, offset // 3 // 1280 + 2))
            return locations

        positions = wait_for(points, 'rendered link rows missing')

        def move(point):
            run('xdotool', 'mousemove', str(point[0]), str(point[1]))
            time.sleep(0.2)

        run('xdotool', 'mousemove', '--window', win, '40', '12')
        time.sleep(2)
        hand = cursor()
        result['hand'] = hand
        move(positions[0])
        arrow = wait_for(lambda: cursor() if cursor() != hand else None,
                         'plain text and pane chip cursors must differ')
        result['arrow'] = arrow
        for name, point, expected in zip(
                ['plain', 'named-osc8', 'detected-url', 'unsafe-target', 'unicode-target'],
                positions, [arrow, hand, hand, arrow, arrow]):
            move(point)
            result['last_cursor'] = cursor()
            result['positions'] = positions
            wait_for(lambda: cursor() == expected, name + ' cursor mismatch')
            result['cases'].append(name)
            if name == 'named-osc8':
                capture(out / 'hand-over-link.png')

        move(positions[1])
        for key, expected, name in [('ctrl+shift+p', arrow, 'palette-blocks-link'),
                                    ('Escape', hand, 'palette-dismiss-restores-link')]:
            run('xdotool', 'key', key)
            time.sleep(2)
            assert cursor() == expected, name + ' cursor mismatch'
            result['cases'].append(name)
        for key, expected, name in [('r', arrow, 'stationary-link-removed'),
                                     ('l', hand, 'stationary-link-restored'),
                                     ('s', arrow, 'stationary-link-scrolled')]:
            run('xdotool', 'key', key)
            time.sleep(2)  # Assert after idle, without moving the pointer.
            assert cursor() == expected, name + ' cursor mismatch'
            result['cases'].append(name)
        assert host.poll() is None, 'host exited during hover checks'
        result['status'] = 'PASS'
        print('HYPERLINK_HOVER_E2E_COMPLETE: ' + ', '.join(result['cases']))
    except Exception as error:
        result['error'] = str(error)
        if host and host.poll() is None:
            capture(out / 'failure.png')
        raise
    finally:
        if display:
            x11.XCloseDisplay(display)
        for process in (host, xvfb):
            if process:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)
        (out / 'result.json').write_text(json.dumps(result, indent=2) + '\n')


if __name__ == '__main__':
    main()
