#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Open several modals in a real window and check the scrim behind each one.

The drive starts a private daemon with one session showing a few lines of
text, opens the app on it, and then opens, one at a time:
- the command palette;
- the theme picker;
- Transparency;
- the switcher;
- the new-space name prompt;
- settings;
- the update panel;
- the Restart to update confirmation, opened from the update panel.

For each one it waits until the scrim has finished fading in, then finds the
margin the modal leaves at the window's left or right edge: blocks of
columns, from the edge inward, whose brightness fell to 40-85% of the frame
before the modal opened (only the scrim does that), less the last block, which
may reach the modal. The window opens 112 cells wide. In that
margin, which must be at least 8 pixels and hold text, the mean brightness
must be 45-85% of before (dimmed) and the edge contrast well below the
dimming alone (blurred). Escape then closes every modal; the first frame
without one must have no scrim and the window's old brightness.

Captures go to OUT: before.png (no modal), one PNG per modal, after.png
(all closed), and with --screenshots the whole screen at each of those
moments (screen-*.png), for the owner. With --key-relay the relay takes the
screen instead (a request {"snap": NAME}), since a macOS guest's console is
what shows the window.

It waits on signals, never on fixed time: the app prints a change line
(PRISMATTYC_UI_EVENTS=1) after each presented frame in which the open modal
or its scrim changed, carrying the frame's dump sequence
(PRISMATTYC_DUMP_PRESENT with {seq}). Keys go to the host as in
update-progress-e2e.py (Quartz events, XTEST, SendInput, or --key-relay).

--system names what the operating system is set to when the drive starts, which
the operator sets first (macOS Accessibility > Display, Windows Settings, GNOME
Settings or gsettings): none (the default), reduce-motion, reduce-transparency
or both. The drive then also requires what the setting changes. With reduce
motion each scrim is already settled on the modal's first change line. With
reduce transparency the scrim dims only: blur false and edge contrast kept.
Reduce motion alone must still blur. The first change line's state is recorded
as `settled_at_once`.

  modal-scrim-e2e.py --bins target/release --out DIR [--key-relay DIR] [--screenshots]
                     [--system none|reduce-motion|reduce-transparency|both]
"""
import argparse
import importlib.util
import json
import os
import shutil
import struct
import subprocess
import sys
import tempfile
import zlib
from pathlib import Path

HERE = Path(__file__).resolve().parent
_spec = importlib.util.spec_from_file_location('update_progress_e2e', HERE / 'update-progress-e2e.py')
_drive = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_drive)
Keys, LineReader, EXE = _drive.Keys, _drive.LineReader, _drive.EXE

UI = 'prismattyc-host: ui: '
# Each name maps to (macOS key code, X keysym, Windows virtual key).
Keys.CODES.update({
    'p': (35, 'p', 0x50), 'k': (40, 'k', 0x4B), 'j': (38, 'j', 0x4A), 'o': (31, 'o', 0x4F),
    'l': (37, 'l', 0x4C), 'i': (34, 'i', 0x49),
})
# No Option/Alt: on macOS, Option+letter can be a dead key.
BINDINGS = {'theme_picker': 'ctrl+shift+k', 'transparency': 'ctrl+shift+j', 'save_space': 'ctrl+shift+l',
            'space_settings': 'ctrl+shift+i', 'update_restart': 'ctrl+shift+y'}
# (capture name, key, modal the app reports)
MODALS = [('palette', 'p', 'palette'), ('theme-picker', 'k', 'theme picker'),
          ('transparency', 'j', 'transparency'), ('switcher', 'o', 'space picker'),
          ('new-space', 'l', 'new space'), ('settings', 'i', 'settings'),
          ('update-panel', 'y', 'space panel')]
WINDOW_CELLS = '112x34'
TEXT = 'the quick brown fox jumps over the lazy dog 0123456789'
DONE = 'SCRIM-READY'


def png_rgb(path):
    """Rows of (r, g, b) tuples from an 8-bit RGB or RGBA PNG."""
    content = path.read_bytes()
    assert content[:8] == b'\x89PNG\r\n\x1a\n', f'{path} is not a PNG'
    pos, data = 8, bytearray()
    while pos < len(content):
        length, kind = struct.unpack('>I4s', content[pos:pos + 8])
        chunk = content[pos + 8:pos + 8 + length]
        pos += 12 + length
        if kind == b'IHDR':
            width, height, depth, color = struct.unpack('>IIBB', chunk[:10])
            assert depth == 8 and color in (2, 6), (depth, color)
            bpp = 3 if color == 2 else 4
        elif kind == b'IDAT':
            data.extend(chunk)
        elif kind == b'IEND':
            break
    raw, stride = zlib.decompress(bytes(data)), width * bpp
    rows, previous, at = [], bytearray(stride), 0
    for _ in range(height):
        kind, line = raw[at], bytearray(raw[at + 1:at + 1 + stride])
        at += 1 + stride
        for i in range(stride):
            left = line[i - bpp] if i >= bpp else 0
            up, corner = previous[i], previous[i - bpp] if i >= bpp else 0
            if kind == 1:
                line[i] = (line[i] + left) & 255
            elif kind == 2:
                line[i] = (line[i] + up) & 255
            elif kind == 3:
                line[i] = (line[i] + (left + up) // 2) & 255
            elif kind == 4:
                p = left + up - corner
                pa, pb, pc = abs(p - left), abs(p - up), abs(p - corner)
                line[i] = (line[i] + (left if pa <= pb and pa <= pc else up if pb <= pc else corner)) & 255
        rows.append([tuple(line[x * bpp:x * bpp + 3]) for x in range(width)])
        previous = line
    return rows


def band_metrics(rows, columns):
    """Mean brightness and mean horizontal edge contrast over `columns` (a
    range of x) for the whole height."""
    total = edges = count = 0
    for row in rows:
        values = [sum(row[x]) / 3 for x in columns]
        total += sum(values)
        edges += sum(abs(b - a) for a, b in zip(values, values[1:]))
        count += len(values)
    return total / count, edges / count


def free_margin(before, after):
    """The widest band at the window's left or right edge that the modal
    leaves uncovered. From the edge inward, in blocks of 1/128 of the width
    (wider than the blur spreads a thin line), take the blocks whose mean
    brightness fell to between 40% and 85% of before, as only the scrim makes
    it, and leave out the last one, which may reach the modal."""
    width = len(before[0])
    step = max(4, width // 128)

    def mean(rows, columns):
        return sum(sum(row[x]) for row in rows for x in columns) / (3 * len(rows) * len(columns))

    best = range(0)
    for side in ('left', 'right'):
        taken = 0
        while (taken + 1) * step <= width // 4:
            block = range(taken * step, (taken + 1) * step)
            if side == 'right':
                block = range(width - (taken + 1) * step, width - taken * step)
            was = mean(before, block)
            if not (was > 0 and 0.40 <= mean(after, block) / was <= 0.85):
                break
            taken += 1
        taken = max(0, taken - 1)
        band = range(0, taken * step) if side == 'left' else range(width - taken * step, width)
        if len(band) > len(best):
            best = band
    return best


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--bins', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--key-relay', type=Path)
    parser.add_argument('--screenshots', action='store_true', help='also capture the whole screen')
    parser.add_argument('--system', choices=['none', 'reduce-motion', 'reduce-transparency', 'both'], default='none',
                        help='the system accessibility setting in force; the drive checks its effect')
    args = parser.parse_args()
    expect_still = args.system in ('reduce-motion', 'both')
    expect_flat = args.system in ('reduce-transparency', 'both')
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=False)
    frames = out / 'frames'
    frames.mkdir()
    for name in ('home', 'config', 'data', 'state', 'project'):
        (out / name).mkdir()
    bins = args.bins.resolve()
    runtime = Path(tempfile.mkdtemp(prefix='pscrim-'))
    config = out / 'config.toml'
    config.write_text('splash = false\nstart_at_login = false\nautomatic_update_checks = false\n'
                      'install_agent_skills = false\nchrome_style = "graphite"\nsettings_panel = true\n'
                      '[keys]\n' + ''.join(f'{action} = "{chord}"\n' for action, chord in BINDINGS.items())
                      + ('[mux]\nshell = "cmd.exe"\n' if os.name == 'nt' else ''))
    env = {k: v for k, v in os.environ.items() if not k.startswith(('PMUX', 'PRISMATTYC_', 'XDG_'))}
    socket_path = None if os.name == 'nt' else runtime / 'prismattyc' / 'pmux.sock'
    if socket_path:
        socket_path.parent.mkdir(mode=0o700)
    env.update(PMUX=str(bins / ('pmux' + EXE)), PRISMATTYC_CONFIG=str(config), PRISMATTYC_NO_AGENT_SKILLS='1',
               PRISMATTYC_UI_EVENTS='1', PRISMATTYC_DUMP_PRESENT=str(frames / 'frame-{seq}.png'),
               # Wide enough that every modal leaves a margin beside it.
               PRISMATTYC_E2E_WINDOW_CELLS=WINDOW_CELLS)
    if socket_path:
        env['PMUX_SOCKET'] = str(socket_path)
    if os.name == 'nt':
        env.update(USERPROFILE=str(out / 'home'), LOCALAPPDATA=str(out / 'data'), APPDATA=str(out / 'config'))
    else:
        env.update(HOME=str(out / 'home'), SHELL='/bin/sh', XDG_RUNTIME_DIR=str(runtime),
                   XDG_DATA_HOME=str(out / 'data'), XDG_CONFIG_HOME=str(out / 'config'),
                   XDG_STATE_HOME=str(out / 'state'))
    env['PATH'] = str(bins) + os.pathsep + env['PATH']
    result = {'status': 'FAIL', 'platform': sys.platform, 'system': args.system, 'checks': [], 'modals': {}}
    owned, readers = [], []
    keys = app = None

    def cli(*words, timeout=60):
        socket = ['--socket', str(socket_path)] if socket_path else []
        p = subprocess.run([str(bins / ('pmux' + EXE)), *socket, *words], env=env, cwd=out / 'project',
                           capture_output=True, text=True, timeout=timeout)
        if p.returncode:
            raise AssertionError(f'pmux {" ".join(words)} exited {p.returncode}: {p.stdout}{p.stderr}')
        return p.stdout

    def record(check):
        result['checks'].append(check)
        print('PASS ' + check, flush=True)

    def change(test, label, timeout=60):
        """The next change line whose state passes `test`."""
        line = app.await_line(lambda line: line.startswith(UI) and test(json.loads(line[len(UI):])['ui']),
                              label, timeout)
        return json.loads(line[len(UI):])

    def frame(seq):
        return frames / f'frame-{seq:06d}.png'

    def screenshot(name):
        if not args.screenshots:
            return
        path = out / f'screen-{name}.png'
        if args.key_relay:
            request = (json.dumps({'snap': name}) + '\n').encode()
            assert os.write(keys.requests, request) == len(request), f'short relay write for {name}'
            keys._await_ack(name)
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

    def await_text(pane):
        """Wait until the daemon shows the text's end marker in the pane."""
        socket = ['--socket', str(socket_path)] if socket_path else []
        watch = subprocess.Popen([str(bins / ('pmux' + EXE)), *socket, 'attach', 'work', '--watch', '--json',
                                  '--pane', pane], env=env, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                 text=True, errors='replace')
        reader = LineReader(watch, out / 'watch.log', lambda line: DONE + '-7' in line)
        try:
            reader.await_line(lambda line: DONE + '-7' in line, 'text in the pane', 60)
        finally:
            watch.terminate()
            watch.wait(10)
            reader.thread.join(10)

    def open_modal(name, key, kind):
        keys.press(key, ctrl=True, shift=True)
        opened = change(lambda ui: ui.get('modal') == kind, f'{name} open')
        # The frame presented just before the modal's first one.
        before_seq = opened['seq'] - 1
        return before_seq, settle(name, kind, before_seq, opened)

    def settle(name, kind, before_seq, opened):
        at_once = bool((opened['ui'].get('scrim') or {}).get('settled'))
        assert at_once or not expect_still, f'{name}: the scrim faded in although the system asks for reduced motion'
        settled = opened if at_once else change(
            lambda ui: ui.get('modal') == kind and (ui.get('scrim') or {}).get('settled'), f'{name} scrim settled')
        shutil.copy(frame(settled['seq']), out / f'{name}.png')
        screenshot(name)
        before_rows, after_rows = png_rgb(frame(before_seq)), png_rgb(frame(settled['seq']))
        band = free_margin(before_rows, after_rows)
        assert len(band) >= 8, f'{name}: no dimmed margin beside the modal ({len(band)} columns)'
        before, after = band_metrics(before_rows, band), band_metrics(after_rows, band)
        assert before[1] > 0.3, f'{name}: the margin has no text to blur ({before[1]:.2f})'
        dim, edge = after[0] / before[0], after[1] / before[1]
        side = 'left' if band.start == 0 else 'right'
        blur = settled['ui']['scrim']['blur']
        result['modals'][name] = {'modal': kind, 'margin': [band.start, band.stop], 'brightness': round(dim, 3),
                                  'edges': round(edge, 3), 'blur': blur, 'settled_at_once': at_once, 'before_seq': before_seq,
                                  'seq': settled['seq']}
        assert 0.45 <= dim <= 0.85, f'{name}: margin brightness went to {dim:.2f} of before'
        if expect_flat:
            assert not blur, f'{name}: the scrim blurred although the system asks for reduced transparency'
            assert edge >= dim * 0.8, f'{name}: edges fell to {edge:.2f} against brightness {dim:.2f}; the frame was blurred'
            record(f'{name}: {side} margin ({len(band)} px) dimmed to {dim:.2f}, not blurred (edges {edge:.2f}), '
                   f'{"at once" if at_once else "faded in"} behind the {kind}')
            return settled
        assert blur, f'{name}: the scrim fell back to dim only'
        assert edge < dim * 0.8, f'{name}: edges kept {edge:.2f} against brightness {dim:.2f}; no blur'
        record(f'{name}: {side} margin ({len(band)} px) dimmed to {dim:.2f} and blurred (edges {edge:.2f}), '
               f'{"at once" if at_once else "faded in"} behind the {kind}')
        return settled

    def close_all(label):
        while True:
            keys.press('Escape')
            closed = change(lambda ui: True, f'{label} after Escape')
            if closed['ui'].get('modal') is None:
                assert closed['ui'].get('scrim') is None, closed
                return closed

    try:
        socket = ['--socket', str(socket_path)] if socket_path else []
        daemon = subprocess.Popen([str(bins / ('pmuxd' + EXE)), *socket], env=env, cwd=out / 'project',
                                  stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, errors='replace')
        owned.append(daemon)
        daemon_out = LineReader(daemon, out / 'daemon.log', lambda line: True)
        readers.append(daemon_out)
        daemon_out.await_line(lambda line: line.strip().endswith('pmux.sock'), 'daemon socket line', 60)
        cli('new', '--no-attach', 'work')
        pane = next(line.split()[1] for line in cli('ls').splitlines() if line.split()[:1] == ['pane'])
        # Eight lines of text for the scrim to dim and blur, then a marker the
        # typed command does not contain.
        if os.name == 'nt':
            typed = f'for /L %i in (1,1,8) do @echo line %i {TEXT} & echo {DONE}-^7'
        else:
            typed = f'for i in 1 2 3 4 5 6 7 8; do echo "line $i {TEXT}"; done; echo {DONE}-$((7))'
        cli('send', pane, typed, '--enter')
        await_text(pane)
        host = subprocess.Popen([str(bins / ('prismattyc-host' + EXE)), '--no-splash', '--attach-session', 'work'],
                                env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
                                errors='replace')
        owned.append(host)
        app = LineReader(host, out / 'host.log', lambda line: line.startswith(UI))
        readers.append(app)
        first = change(lambda ui: True, 'first presented frame')
        assert first['ui'].get('modal') is None and first['ui'].get('scrim') is None, first
        keys = Keys(host.pid, args.key_relay)
        # Open and close the palette once, so the window has settled on the
        # attached session before the first modal is measured.
        keys.press('p', ctrl=True, shift=True)
        change(lambda ui: ui.get('modal') == 'palette', 'warm-up palette')
        last = close_all('warm-up palette')
        screenshot('before')
        for name, key, kind in MODALS:
            before_seq, settled = open_modal(name, key, kind)
            if name == MODALS[0][0]:
                shutil.copy(frame(before_seq), out / 'before.png')
            if name == 'update-panel':
                # The confirmation: Update and restart from the panel's rows.
                rows = [row.removesuffix(': ') for row in
                        (json.loads(cli('render-status', '--json'))['windows'][0].get('space_details') or {})
                        .get('rows', [])]
                target = next(i for i, row in enumerate(rows) if row.startswith('Update and restart'))
                selected = (settled['ui'].get('panel') or {}).get('selected')
                while selected != target:
                    keys.press('Down' if target > selected else 'Up')
                    selected = (change(lambda ui: (ui.get('panel') or {}).get('selected') not in (None, selected),
                                       'panel selection')['ui'].get('panel') or {}).get('selected')
                keys.press('Return')
                confirm = change(lambda ui: ui.get('modal') == 'restart', 'restart confirmation', 120)
                settle('restart-confirmation', 'restart', before_seq, confirm)
            last = close_all(name)
            was_rows = png_rgb(frame(before_seq))
            everything = range(len(was_rows[0]))
            was = band_metrics(was_rows, everything)
            now = band_metrics(png_rgb(frame(last['seq'])), everything)
            assert abs(now[0] / was[0] - 1) < 0.06, f'{name}: closed, the window is {now[0] / was[0]:.2f} as bright as before'
        shutil.copy(frame(last['seq']), out / 'after.png')
        screenshot('after')
        record(f'every modal closed with Escape and left no scrim ({len(result["modals"])} modals)')
        result['status'] = 'PASS'
    except Exception as error:
        result['error'] = f'{type(error).__name__}: {error}'
        print('FAIL ' + result['error'], flush=True)
    finally:
        if keys:
            keys.close()
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
    print(json.dumps({k: result[k] for k in ('status', 'platform')}))
    return 0 if result['status'] == 'PASS' else 1


if __name__ == '__main__':
    sys.exit(main())
