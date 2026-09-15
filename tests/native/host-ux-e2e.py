#!/usr/bin/env python3
"""Drive the real host in the native test container. Missing windows or pixels fail closed."""
import collections
import hashlib
import json
import os
from pathlib import Path
import shutil
import struct
import subprocess
import time
import zlib

FOCUS = (155, 140, 245)
RESTORE_MARKER = (17, 231, 149)
OUT = Path(os.environ.get("HOST_UX_OUT", "/tmp/host-ux-e2e"))


def run(*args):
    return subprocess.check_output(args, text=True).strip()


def wait_for(check, label, timeout=10):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            value = check()
            if value:
                return value
        except (OSError, ValueError, KeyError, subprocess.CalledProcessError):
            pass
        time.sleep(.05)
    raise AssertionError(f"timed out: {label}")


class PNG:
    def __init__(self, content):
        assert content[:8] == b"\x89PNG\r\n\x1a\n", "missing PNG signature"
        pos, data, ended = 8, bytearray(), False
        while pos < len(content):
            length, kind = struct.unpack(">I4s", content[pos:pos + 8])
            chunk = content[pos + 8:pos + 8 + length]
            crc = struct.unpack(">I", content[pos + 8 + length:pos + 12 + length])[0]
            assert zlib.crc32(kind + chunk) == crc, "incomplete or corrupt PNG"
            pos += 12 + length
            if kind == b"IHDR":
                self.width, self.height, depth, color, compression, filtering, interlace = struct.unpack(">IIBBBBB", chunk)
                assert depth == 8 and color in (2, 6) and not (compression or filtering or interlace)
                self.bpp = 3 if color == 2 else 4
            elif kind == b"IDAT":
                data.extend(chunk)
            elif kind == b"IEND":
                ended = True
                break
        assert ended, "PNG has no completion chunk"
        stride = self.width * self.bpp
        raw = zlib.decompress(data)
        assert len(raw) == self.height * (stride + 1)
        self.rows, previous, pos = [], bytes(stride), 0
        for _ in range(self.height):
            filt = raw[pos]
            row = bytearray(raw[pos + 1:pos + 1 + stride])
            pos += stride + 1
            assert filt in range(5)
            if filt:
                for i in range(stride):
                    left = row[i - self.bpp] if i >= self.bpp else 0
                    up = previous[i]
                    corner = previous[i - self.bpp] if i >= self.bpp else 0
                    if filt == 1:
                        value = left
                    elif filt == 2:
                        value = up
                    elif filt == 3:
                        value = (left + up) // 2
                    else:
                        p = left + up - corner
                        distances = abs(p - left), abs(p - up), abs(p - corner)
                        value = (left, up, corner)[distances.index(min(distances))]
                    row[i] = (row[i] + value) & 255
            previous = bytes(row)
            self.rows.append(previous)

    def pixel(self, x, y):
        assert 0 <= x < self.width and 0 <= y < self.height
        offset = x * self.bpp
        return tuple(self.rows[y][offset:offset + 3])


def focus_bounds(frame):
    columns, rows = collections.Counter(), collections.Counter()
    for y, row in enumerate(frame.rows):
        for x in range(frame.width):
            if row[x * frame.bpp:x * frame.bpp + 3] == bytes(FOCUS):
                columns[x] += 1
                rows[y] += 1
    assert len(columns) >= 2 and len(rows) >= 2, "no focused border"
    xs, ys = columns.most_common(2), rows.most_common(2)
    x0, x1 = sorted(x for x, _ in xs)
    y0, y1 = sorted(y for y, _ in ys)
    assert x1 - x0 >= frame.width * .25 and y1 - y0 >= frame.height * .5, "no full pane border"
    assert all(n >= (y1 - y0) * .95 for _, n in xs), "fragmented vertical border"
    assert all(n >= (x1 - x0) * .95 for _, n in ys), "fragmented horizontal border"
    return x0, y0, x1, y1


def cursor_geometry(frame, bounds):
    x0, y0, x1, y1 = bounds
    points = set()
    for y in range(y0 + 8, min(y0 + 90, y1 - 8)):
        for x in range(x0 + 8, x1 - 8):
            rgb = frame.pixel(x, y)
            if min(rgb) >= 180 and max(rgb) - min(rgb) <= 2:
                points.add((x, y))
    candidates = []
    while points:
        component, todo = [], [points.pop()]
        while todo:
            x, y = todo.pop()
            component.append((x, y))
            for point in ((x - 1, y), (x + 1, y), (x, y - 1), (x, y + 1)):
                if point in points:
                    points.remove(point)
                    todo.append(point)
        left, right = min(x for x, _ in component), max(x for x, _ in component)
        top, bottom = min(y for _, y in component), max(y for _, y in component)
        width, height = right - left + 1, bottom - top + 1
        if width >= 3 and height >= 8 and len(component) >= width * height * .95:
            candidates.append((left, top, width, height))
    assert len(candidates) == 1, f"expected one solid blank cursor, got {candidates}"
    x, y, width, height = candidates[0]
    return x, y, width, height, frame.pixel(x, y)


def assert_cursor(frame, geometry, expected, cells, offset=(0, 0)):
    x, y, width, height, color = geometry
    x, y = x + offset[0], y + offset[1]
    densities = [sum(frame.pixel(x + col * width + dx, y + dy) == color
                     for dy in range(height) for dx in range(width)) / (width * height)
                 for col in range(cells)]
    visible = [col for col, density in enumerate(densities) if density >= .55]
    assert visible == [expected], f"caret did not move cleanly: expected cell {expected}, densities={densities}"
    return densities


def border_metrics(frame, bounds, offset=(0, 0)):
    x0, y0, x1, y1 = bounds
    x0, x1 = x0 + offset[0], x1 + offset[0]
    y0, y1 = y0 + offset[1], y1 + offset[1]
    perimeter = ([(x, y0) for x in range(x0, x1 + 1)]
                 + [(x1, y) for y in range(y0 + 1, y1 + 1)]
                 + [(x, y1) for x in range(x1 - 1, x0 - 1, -1)]
                 + [(x0, y) for y in range(y1 - 1, y0, -1)])
    trace = [frame.pixel(x, y) == FOCUS for x, y in perimeter]
    end = max((i + 1 for i, traced in enumerate(trace) if traced), default=0)
    gaps = sum(not traced for traced in trace[:end])
    interior = 0
    for y in range(y0 + 1, y1):
        for x in range(x0 + 1, x1):
            if min(x - x0, x1 - x, y - y0, y1 - y) <= 7:
                interior += frame.pixel(x, y) == FOCUS
    return {"traced": sum(trace), "perimeter": len(trace), "prefix_gaps": gaps, "interior_focus_pixels": interior}


class Host:
    def __init__(self, name, args, config):
        self.directory = OUT / name
        self.directory.mkdir(parents=True)
        self.present = self.directory / 'present.png'
        path = self.directory / 'config.toml'
        path.write_text(config)
        env = dict(os.environ, PRISMATTYC_CONFIG=str(path), PRISMATTYC_DUMP_PRESENT=str(self.present), PS1='FRESH> ')
        env.pop('WAYLAND_DISPLAY', None)
        self.log = (self.directory / 'host.log').open('w')
        self.process = subprocess.Popen(['prismattyc-host', '--no-splash', *args], env=env, stdout=self.log, stderr=subprocess.STDOUT)
        self.wid = wait_for(lambda: run('xdotool', 'search', '--onlyvisible', '--pid', str(self.process.pid)), 'native host window').splitlines()[-1]
        run('xdotool', 'windowsize', '--sync', self.wid, '1000', '600')
        if os.environ.get('HOST_UX_NO_WM') != '1':
            run('xdotool', 'windowactivate', '--sync', self.wid)
        run('xdotool', 'windowfocus', '--sync', self.wid)
        run('xdotool', 'mousemove', '1910', '1070')
        wait_for(lambda: self.present.exists(), 'presented pixels')

    def status(self):
        data = json.loads(run('pmux', 'render-status', '--json'))
        assert data['host_pid'] == self.process.pid, 'render status belongs to a different host'
        return data['windows'][0]

    def key(self, key):
        run('xdotool', 'key', '--clearmodifiers', key)

    def capture(self, name):
        def stable():
            before = self.present.with_suffix('.json').read_bytes()
            pixels = self.present.read_bytes()
            after = self.present.with_suffix('.json').read_bytes()
            return (pixels, json.loads(after)) if before == after else None
        pixels, meta = wait_for(stable, 'stable present snapshot')
        (self.directory / (name + '.png')).write_bytes(pixels)
        (self.directory / (name + '.json')).write_text(json.dumps(meta))
        return pixels, meta

    def wait_cursor(self, name, geometry, expected, cells):
        last_error = None
        def check():
            nonlocal last_error
            pixels, metadata = self.capture(name)
            try:
                densities = assert_cursor(PNG(pixels), geometry, expected, cells)
            except AssertionError as error:
                last_error = error
                return None
            return densities, metadata
        try:
            return wait_for(check, f'presented {name} cursor', timeout=15)
        except AssertionError:
            raise AssertionError(f'{name}: {last_error}') from None

    def display(self, name):
        path = self.directory / (name + '-display.png')
        run('ffmpeg', '-hide_banner', '-loglevel', 'error', '-f', 'x11grab', '-video_size', '1920x1080', '-i', os.environ['DISPLAY'], '-frames:v', '1', '-y', str(path))
        values = dict(line.split('=', 1) for line in run('xdotool', 'getwindowgeometry', '--shell', self.wid).splitlines())
        return PNG(path.read_bytes()), (int(values['X']), int(values['Y']))

    def stop(self):
        self.process.terminate()
        self.process.wait(timeout=5)
        self.log.close()


def navigation_and_border(config):
    result_path = OUT / 'readline.txt'
    command = 'printf "\\033[2 q"; IFS= read -r -e -p "NAV> " line; printf "%s" "$line" > "$1"; sleep 180'
    host = Host('navigation', ['--panes', '2', '--', 'bash', '--noprofile', '--norc', '-c', command, 'bash', str(result_path)], config)
    try:
        time.sleep(3.5)
        initial, _ = host.capture('initial')
        frame = PNG(initial)
        geometry = cursor_geometry(frame, focus_bounds(frame))
        run('xdotool', 'type', '--clearmodifiers', '--delay', '50', 'abcd')
        host.wait_cursor('typed', geometry, 4, 6)
        host.key('Left')
        left, metadata = host.wait_cursor('left', geometry, 3, 6)
        assert not metadata['full'], 'Left must exercise partial raster without a bell or overlay'
        display, offset = host.display('left')
        assert_cursor(display, geometry, 3, 6, offset)
        assert not any('bell' in name for name in host.status()['last_raster']['guards']), 'bell affected navigation'
        run('xdotool', 'type', '--clearmodifiers', 'X')
        host.key('Home')
        home, metadata = host.wait_cursor('home', geometry, 0, 6)
        assert not metadata['full'], 'Home must exercise partial raster without a bell or overlay'
        display, offset = host.display('home')
        assert_cursor(display, geometry, 0, 6, offset)
        assert not any('bell' in name for name in host.status()['last_raster']['guards'])
        run('xdotool', 'type', '--clearmodifiers', 'Y')
        host.key('End')
        host.key('Return')
        wait_for(lambda: result_path.exists(), 'Bash readline result')
        assert result_path.read_text() == 'YabcXd', 'child did not receive Left/Home navigation'
        time.sleep(.4)
        host.key('alt+Left')
        frames = []
        deadline = time.monotonic() + 3.5
        while time.monotonic() < deadline:
            time.sleep(.12)
            frames.append(host.capture(f'cycle-{len(frames):02}'))
        time.sleep(.3)
        settled, _ = host.capture('settled')
        final = PNG(settled)
        bounds = focus_bounds(final)
        measurements = [border_metrics(PNG(pixels), bounds) for pixels, _ in frames]
        final_metrics = border_metrics(final, bounds)
        display, offset = host.display('settled')
        display_metrics = border_metrics(display, bounds, offset)
        (host.directory / 'border-metrics.json').write_text(json.dumps({'frames': measurements, 'settled': final_metrics, 'display': display_metrics}, indent=2))
        intermediate = [m['traced'] for m in measurements if .05 < m['traced'] / m['perimeter'] < .95]
        assert len(set(intermediate)) >= 3, 'no moving light-cycle sweep: expected at least three distinct intermediate trails'
        assert all(m['prefix_gaps'] <= 10 for m in measurements), 'light-cycle trail has fragmented gaps'
        assert all(m['interior_focus_pixels'] <= 24 for m in measurements), 'light-cycle leaves stale vehicle heads'
        assert all(a['traced'] <= b['traced'] for a, b in zip(measurements, measurements[1:])), 'light-cycle trail moves backwards'
        for m in [final_metrics, display_metrics]:
            assert m['traced'] == m['perimeter'] and m['interior_focus_pixels'] == 0, 'settled border retains stale pixels'
        return {'cursor_geometry': geometry, 'left_densities': left, 'home_densities': home, 'readline': result_path.read_text(), 'border': final_metrics, 'intermediate_trails': len(set(intermediate))}
    finally:
        host.stop()




def marker_pixels(frame):
    color = bytes(RESTORE_MARKER)
    return sum(row[x:x + 3] == color for row in frame.rows for x in range(0, len(row), frame.bpp))


def cold_start(config):
    names = ['ux-restore-a', 'ux-restore-b']
    for name in names:
        run('pmux', 'new', name, '--no-attach', '--no-agent', '--', 'bash', '--noprofile', '--norc')
    run('pmux', 'space', 'save', 'ux-restore', *names)
    for name in names:
        run('pmux', 'stop', name)
    cache = Path(os.environ['XDG_RUNTIME_DIR']) / 'prismattyc/pmux.attach-tabs.json'
    layout = {'tabs': [{'title': 'Saved A', 'sessions': [names[0]]}, {'title': 'Saved B', 'sessions': [names[1]]}],
              'active_tab': 1, 'focused_session': names[1], 'space': 'ux-restore', 'mode': 'switch'}
    cache.write_text(json.dumps(layout))
    original = cache.read_bytes()
    host = Host('restore-decline', [], config)
    try:
        wait_for(lambda: 'restore-prompt' in host.status()['last_raster']['guards'], 'restore question before regroup')
        host.capture('question')
        assert host.status()['pane_count'] == 1 and cache.read_bytes() == original
        host.key('Escape')
        wait_for(lambda: 'restore-prompt' not in host.status()['last_raster']['guards'], 'declined question')
        time.sleep(2)
        assert host.status()['pane_count'] == 1 and cache.read_bytes() == original, 'decline silently restored saved panes'
        assert 'restore-prompt' not in host.status()['last_raster']['guards'], 'question repeated after decline'
        host.capture('fresh')
        host.display('fresh')
    finally:
        host.stop()
    host = Host('restore-accept', [], config)
    try:
        wait_for(lambda: 'restore-prompt' in host.status()['last_raster']['guards'], 'question on next cold launch')
        host.capture('question')
        host.key('Return')
        wait_for(lambda: 'restore-prompt' not in host.status()['last_raster']['guards']
                 and '2 tabs' in run('xdotool', 'getwindowname', host.wid), 'restored saved tab layout')
        time.sleep(2)
        assert '2 tabs' in run('xdotool', 'getwindowname', host.wid), 'idle refresh discarded saved exited sessions'
        host.capture('exited-panes')
        host.key('Return')
        wait_for(lambda: names[1] in run('pmux', 'ls'), 'Enter revives saved focused session')
        command = r"printf '\033[48;2;17;231;149mRESTORED_READY\033[0m\n'"
        run('xdotool', 'type', '--clearmodifiers', '--delay', '40', command)
        host.key('Return')
        def live_output():
            pixels, _ = host.capture('revived')
            count = marker_pixels(PNG(pixels))
            return count if count > 200 else None
        rendered = wait_for(live_output, 'revived guest output in the host', timeout=20)
        display, _ = host.display('revived')
        assert marker_pixels(display) > 200, 'revived output is absent from the X11 display'
    finally:
        host.stop()
    return {'decline_stays_fresh': True, 'cache_preserved': True, 'next_launch_asks': True,
            'accept_restores_two_tabs': True, 'enter_revives_focused_session': True, 'revived_marker_pixels': rendered}


def main():
    OUT.mkdir(parents=True, exist_ok=True)
    config = ('theme = "prismattyc-default"\nfont_px = 16.0\nfocus_border = "violet"\n'
              f'focus_border_animation = "{os.environ.get("HOST_UX_ANIMATION", "light-cycle")}"\n'
              'focus_border_animation_ms = 3000\nwindow_padding_px = 5\npane_gap_px = 5\npane_padding_px = 5\n'
              'bell_toaster = true\nbell_toaster_ms = 10000\n'
              'render_timer = "log"\nrender_timer_log_every_frame = true\n')
    result = {'host_binary_sha256': hashlib.sha256(Path(shutil.which('prismattyc-host')).read_bytes()).hexdigest()}
    try:
        result['navigation_and_border'] = navigation_and_border(config)
        result['cold_start'] = cold_start(config)
        result['status'] = 'PASS'
        print('HOST_UX_E2E_COMPLETE: caret, light-cycle, and restore PASS', flush=True)
    except Exception as error:
        result['status'] = 'FAIL'
        result['error'] = str(error)
        raise
    finally:
        (OUT / 'result.json').write_text(json.dumps(result, indent=2) + '\n')


if __name__ == '__main__':
    main()
