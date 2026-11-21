#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Drive isolated native-host windows using filesystem and control-socket signals."""
import argparse
import hashlib
import base64
import collections
import ctypes
import importlib.util
import json
import os
import re
from pathlib import Path
import select
import shlex
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time

parser = argparse.ArgumentParser()
parser.add_argument('--out', type=Path, required=True)
parser.add_argument('--bins', type=Path, required=True)
parser.add_argument('--probe', type=Path, required=True)
parser.add_argument('--repo', type=Path, required=True)
parser.add_argument('--presenter', choices=('tiles', 'iosurface'), default='tiles')
parser.add_argument('--native', choices=('on', 'off'), default='on')
parser.add_argument('--lifecycle', choices=('images', 'text', 'ctrl-z', 'lease', 'metrics', 'metrics-live', 'budgets', 'animation'), default='images')
parser.add_argument('--window-cells')
args = parser.parse_args()
producer_mode = '--direct-local-alt' if args.lifecycle == 'text' else '--direct-local'

def main_screen_scale():
    ctypes.CDLL('/System/Library/Frameworks/AppKit.framework/AppKit')
    objc = ctypes.CDLL('/usr/lib/libobjc.A.dylib')
    objc.objc_getClass.argtypes = [ctypes.c_char_p]
    objc.objc_getClass.restype = ctypes.c_void_p
    objc.sel_registerName.argtypes = [ctypes.c_char_p]
    objc.sel_registerName.restype = ctypes.c_void_p
    object_message = ctypes.CFUNCTYPE(ctypes.c_void_p, ctypes.c_void_p, ctypes.c_void_p)(('objc_msgSend', objc))
    double_message = ctypes.CFUNCTYPE(ctypes.c_double, ctypes.c_void_p, ctypes.c_void_p)(('objc_msgSend', objc))
    screen = object_message(objc.objc_getClass(b'NSScreen'), objc.sel_registerName(b'mainScreen'))
    assert screen, 'native fixture needs an active main display'
    scale = double_message(screen, objc.sel_registerName(b'backingScaleFactor'))
    assert scale in (1.0, 2.0), ('unsupported display backing scale', scale)
    return scale

out = args.out.resolve()
out.mkdir(parents=True, exist_ok=False)
root = args.repo.resolve()
bins = args.bins.resolve()
probe = args.probe.resolve()
desktop_spec = importlib.util.spec_from_file_location('native_desktop', root / 'tests/native/macos-settings-e2e.py')
desktop_module = importlib.util.module_from_spec(desktop_spec)
desktop_spec.loader.exec_module(desktop_module)
try:
    desktop = desktop_module.Desktop()
    desktop.prerequisite()
except (RuntimeError, AssertionError) as error:
    blocked = {'status': 'BLOCKED', 'prerequisite': str(error), 'host_started': False, 'daemon_started': False}
    (out / 'result.json').write_text(json.dumps(blocked, indent=2))
    print(json.dumps(blocked), flush=True)
    raise SystemExit(2)
screen_scale = main_screen_scale()
initial_font_px = 36.0 / screen_scale
runtime = Path(tempfile.mkdtemp(prefix='ncp-', dir='/tmp'))
spec = importlib.util.spec_from_file_location('host_ux', root / 'tests/native/host-ux-e2e.py')
png_module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(png_module)
env = {k: v for k, v in os.environ.items() if not k.startswith(('PMUX', 'PRISMATTYC_', 'XDG_'))}
for shell_startup in ('BASH_ENV', 'ENV', 'PROMPT_COMMAND'):
    env.pop(shell_startup, None)
(out / 'home').mkdir()
env.update(HOME=str(out / 'home'), SHELL='/bin/bash', PATH=f'{bins}:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin',
           PMUX_SOCKET=str(runtime / 'pmux.sock'), XDG_RUNTIME_DIR=str(runtime),
           XDG_CONFIG_HOME=str(out / 'config'), XDG_DATA_HOME=str(out / 'data'), XDG_STATE_HOME=str(out / 'state'),
           PRISMATTYC_CONFIG=str(out / 'config.toml'), PRISMATTYC_DUMP_PRESENT=str(out / 'present.png'))
if args.window_cells:
    assert re.fullmatch(r'[1-9][0-9]*x[1-9][0-9]*', args.window_cells)
    env['PRISMATTYC_E2E_WINDOW_CELLS'] = args.window_cells
(out / 'config.toml').write_text('splash = false\nautomatic_update_checks = false\nstart_at_login = false\nspace_startup = "fresh"\ninstall_agent_skills = false\n' + f'font_px = {initial_font_px:.6f}\n' + 'window_opacity = 1.0\nwindow_padding_px = 0\npane_padding_px = 0\nfocus_border_animation = "none"\n')
with (out / 'config.toml').open('a') as configuration:
    configuration.write(f'macos_present = "{args.presenter}"\n')
(out / 'binary-sha256.json').write_text(json.dumps({str(path.name): hashlib.sha256(path.read_bytes()).hexdigest() for path in [bins / 'prismattyc-host', bins / 'pmuxd', probe]}, indent=2))
host = daemon = control = listener = None
owned_probes = set()
shell_pid = None
events = []
physical_cell = [22, 49]
result = {'status': 'FAIL', 'cases': [], 'profile': 'supplied-binaries', 'presenter': args.presenter, 'backing_scale': screen_scale, 'native_flag': args.native, 'lifecycle': args.lifecycle}
kqueue = select.kqueue()
directory_fds = [os.open(path, os.O_RDONLY) for path in (out, runtime)]
for descriptor in directory_fds:
    kqueue.control([select.kevent(descriptor, filter=select.KQ_FILTER_VNODE,
                                 flags=select.KQ_EV_ADD | select.KQ_EV_CLEAR,
                                 fflags=select.KQ_NOTE_WRITE | select.KQ_NOTE_RENAME | select.KQ_NOTE_EXTEND)], 0, 0)

def health():
    if host is not None and host.poll() is not None:
        raise AssertionError('host exited: ' + (out / 'host.log').read_text())
    if daemon is not None and daemon.poll() is not None:
        raise AssertionError('daemon exited: ' + (out / 'daemon.log').read_text())

def await_file(probe_value, label, bound=10):
    deadline = time.monotonic() + bound
    while True:
        health()
        value = probe_value()
        if value:
            return value
        remaining = deadline - time.monotonic()
        assert remaining > 0, label
        assert kqueue.control(None, 4, remaining), label

def current_frame():
    try:
        before = (out / 'present.json').read_bytes()
        png = (out / 'present.png').read_bytes()
        after = (out / 'present.json').read_bytes()
        if before != after:
            return None
        image = png_module.PNG(png)
        assert image.bpp == 4
        return png, json.loads(after), collections.Counter(image.rows[y][x:x+4] for y in range(image.height) for x in range(0, image.width * 4, 4))
    except (FileNotFoundError, json.JSONDecodeError):
        return None

def presented_frame(sequence, placements, position, old_position=None, ink=False, opacity=255):
    try:
        status = json.loads((runtime / 'pmux.host.render.json').read_text())
        assert status['host_pid'] == host.pid
        window = status['windows'][0]
        pane = window['current_panes'][0]
        installed = (pane.get('native') or {}).get('installed_frame')
        if sequence is None:
            authority = pane.get('native')
            if pane['display_source'] != 'classic' or (authority is not None if args.native == 'off' else not authority or not authority['available'] or authority['installed_frame'] is not None):
                return None
        elif pane['display_source'] != 'native' or not installed or installed['sequence'] != sequence or installed['placements'] != placements:
            return None
        frame = current_frame()
        if not frame:
            return None
        png, meta, counts = frame
        if meta['host_instance'] != status['host_instance'] or not window['last_raster']['present_succeeded'] or window['last_raster']['capture_seq'] != meta['seq']:
            return None
        assert window['last_raster']['present']['backend'] == args.presenter, 'requested presenter was not actually used'
        image = png_module.PNG(png)
        _, (origin_x, origin_y, _, _) = window['visible_pane_content_rects'][0]
        # Fixed 12x4 source, Cover into eight 22x49 cells by four rows:
        # the literal crop is [4,0,4,4], yielding four equal color quadrants.
        cell_width, cell_height = physical_cell
        width, height = 8 * cell_width, 4 * cell_height
        x0, y0 = origin_x + position[0] * cell_width, origin_y + position[1] * cell_height
        colors = ((255, 0, 0), (0, 0, 255), (255, 255, 0), (0, 255, 255))
        if opacity == 153:
            colors = ((153, 0, 0), (0, 0, 153), (153, 153, 0), (0, 153, 153))
        white_ink = 0
        for y in range(height):
            for x in range(width):
                if ink and x < cell_width and y < cell_height:
                    white_ink += image.pixel(x0 + x, y0 + y) == (255, 255, 255)
                    continue
                expected = colors[(2 if y >= height // 2 else 0) + (1 if x >= width // 2 else 0)] if placements else (0, 0, 0)
                if image.pixel(x0 + x, y0 + y) != expected:
                    return None
        if ink and white_ink < 20:
            return None
        if old_position is not None:
            old_x, old_y = origin_x + old_position[0] * cell_width, origin_y + old_position[1] * cell_height
            for y in range(height):
                for x in range(width):
                    if image.pixel(old_x + x, old_y + y) != (0, 0, 0):
                        return None
        (out / f'native-frame-{sequence}-status.json').write_text(json.dumps(status, indent=2))
        return frame
    except (FileNotFoundError, json.JSONDecodeError):
        return None

def white_image_frame(sequence, placements, positions):
    try:
        status = json.loads((runtime / 'pmux.host.render.json').read_text())
        assert status['host_pid'] == host.pid
        window = status['windows'][0]
        pane = window['current_panes'][0]
        installed = (pane.get('native') or {}).get('installed_frame')
        if pane['display_source'] != 'native' or not installed or installed['sequence'] != sequence or installed['placements'] != placements:
            return None
        frame = current_frame()
        if not frame:
            return None
        png, meta, _ = frame
        if meta['host_instance'] != status['host_instance'] or not window['last_raster']['present_succeeded'] or window['last_raster']['capture_seq'] != meta['seq']:
            return None
        assert window['last_raster']['present']['backend'] == args.presenter
        image = png_module.PNG(png)
        _, (origin_x, origin_y, _, _) = window['visible_pane_content_rects'][0]
        for left, top in positions:
            for y in range(196):
                for x in range(176):
                    if image.pixel(origin_x + left * 22 + x, origin_y + top * 49 + y) != (255, 255, 255):
                        return None
        (out / f'budget-frame-{sequence}-status.json').write_text(json.dumps(status, indent=2))
        return frame
    except (FileNotFoundError, json.JSONDecodeError):
        return None

buffer = bytearray()
def event(predicate, label, allow_fallback=False):
    deadline = time.monotonic() + 5
    while True:
        while b'\n' in buffer:
            line, _, remaining = buffer.partition(b'\n')
            buffer[:] = remaining
            value = json.loads(line)
            events.append(value)
            if value.get('event') == 'probe-ready':
                owned_probes.add(value['pid'])
            assert allow_fallback or value.get('event') != 'fallback', value
            if predicate(value):
                return value
        health()
        remaining = deadline - time.monotonic()
        assert remaining > 0 and select.select([control], [], [], remaining)[0], label
        chunk = control.recv(8192)
        assert chunk, label + ': probe disconnected'
        buffer.extend(chunk)

def command(text):
    control.sendall(text.encode() + b'\n')

def window_status():
    try:
        value = json.loads((runtime / 'pmux.host.render.json').read_text())
        assert value['host_pid'] == host.pid
        return value['windows'][0]
    except (FileNotFoundError, json.JSONDecodeError):
        return None

def accessibility_snapshot():
    application = desktop.ax.AXUIElementCreateApplication(host.pid)
    assert application, 'owned host has no Accessibility application element'
    entries = []
    def visit(node, depth=0):
        assert depth < 32 and len(entries) < 4096, 'unexpected owned Accessibility tree size'
        entry = {'depth': depth}
        for attribute in ('AXRole', 'AXTitle', 'AXValue', 'AXDescription'):
            value = desktop.attr(node, attribute)
            if value:
                try:
                    text = desktop.text(value)
                    if text:
                        entry[attribute] = text
                finally:
                    desktop.cf.CFRelease(value)
        entries.append(entry)
        children = desktop.attr(node, 'AXChildren')
        if children:
            try:
                for index in range(desktop.cf.CFArrayGetCount(children)):
                    visit(desktop.cf.CFArrayGetValueAtIndex(children, index), depth + 1)
            finally:
                desktop.cf.CFRelease(children)
    try:
        visit(application)
    finally:
        desktop.cf.CFRelease(application)
    return entries

def accessible_frame(sequence, required, forbidden=()):
    window = window_status()
    frame = current_frame()
    if not window or not frame:
        return None
    pane = window['current_panes'][0]
    installed = (pane.get('native') or {}).get('installed_frame')
    source = 'classic' if sequence is None else 'native'
    if pane['display_source'] != source:
        return None
    if sequence is not None and (not installed or installed['sequence'] != sequence or installed['placements'] != 0):
        return None
    png, meta, _ = frame
    assert meta['host_pid'] == host.pid
    raster = window['last_raster']
    if not raster['present_succeeded'] or raster['capture_seq'] != meta['seq']:
        return None
    assert raster['present']['backend'] == args.presenter
    entries = accessibility_snapshot()
    values = '\n'.join(entry.get('AXValue', '') for entry in entries)
    if not all(text in values for text in required) or any(text in values for text in forbidden):
        return None
    (out / f'accessibility-{source}.json').write_text(json.dumps(entries, ensure_ascii=False, indent=2))
    (out / f'accessibility-{source}.png').write_bytes(png)
    return {'name': source + ' Accessibility text', 'source': source, 'frame': sequence, 'present': meta}

def run_text():
    # Reading the actual application tree activates AccessKit before the new scene.
    accessibility_snapshot()
    command('text-scene')
    changed = event(lambda value: value.get('event') == 'frame' and value.get('placements') == 0, 'native text scene')
    expected = 'NATIVE-CLUSTER 界e' + '\u0301' * 20
    result['cases'].append(await_file(lambda: accessible_frame(changed['frame'], (expected, 'https://example.test/native')),
                                      'native Accessibility preserves the complete wide and combining text'))
    command('release-hold')
    event(lambda value: value.get('event') == 'released', 'release exposes retained Classic text')
    result['cases'].append(await_file(lambda: accessible_frame(None, ('CLASSIC-BASELINE',), ('NATIVE-CLUSTER', 'example.test')),
                                      'Accessibility switches to retained Classic text'))
    result['status'] = 'PASS'

def native_ctrl_z():
    quartz = ctypes.CDLL('/System/Library/Frameworks/ApplicationServices.framework/ApplicationServices')
    quartz.AXIsProcessTrusted.restype = ctypes.c_bool
    assert quartz.AXIsProcessTrusted(), 'native keys need existing Accessibility permission'
    quartz.CGEventCreateKeyboardEvent.argtypes = [ctypes.c_void_p, ctypes.c_uint16, ctypes.c_bool]
    quartz.CGEventCreateKeyboardEvent.restype = ctypes.c_void_p
    quartz.CGEventSetFlags.argtypes = [ctypes.c_void_p, ctypes.c_uint64]
    quartz.CGEventPostToPid.argtypes = [ctypes.c_int, ctypes.c_void_p]
    quartz.CFRelease.argtypes = [ctypes.c_void_p]
    running = desktop.running(desktop.cls(b'NSRunningApplication'), desktop.sel(b'runningApplicationWithProcessIdentifier:'), host.pid)
    assert running and desktop.activate(running, desktop.sel(b'activateWithOptions:'), 2), 'owned host activation failed'
    await_file(lambda: (window if (window := window_status()) and window['focused'] else None), 'owned host focus event')
    desktop.require_focused(host.pid)
    for pressed in (True, False):
        key = quartz.CGEventCreateKeyboardEvent(None, 6, pressed)
        assert key
        quartz.CGEventSetFlags(key, 1 << 18)
        quartz.CGEventPostToPid(host.pid, key)
        quartz.CFRelease(key)

def run_lifecycle(active, ready):
    global control, shell_pid
    status = await_file(window_status, 'native owner status')
    authority = status['current_panes'][0]['native']
    shell_pid = authority['child_pid']
    root_identity = (shell_pid, authority['child_birth'])
    first_pid = next(value['pid'] for value in events if value.get('event') == 'probe-ready')
    assert int(subprocess.check_output(['/bin/ps', '-p', str(first_pid), '-o', 'ppid='])) == shell_pid
    jobs = runtime / 'jobs.txt'
    if args.lifecycle == 'ctrl-z':
        native_ctrl_z()
        event(lambda value: value.get('event') == 'suspending', 'native Ctrl-Z reached the foreground app')
    elif args.lifecycle in ('metrics', 'metrics-live'):
        command('heartbeat')
        event(lambda value: value.get('event') == 'heartbeat-complete', 'finish the operation before changing font metrics')
        if args.lifecycle == 'metrics':
            os.kill(first_pid, signal.SIGSTOP)
        config = out / 'config.toml'
        text = config.read_text()
        setting = f'font_px = {initial_font_px:.6f}'
        assert text.count(setting) == 1
        config.write_text(text.replace(setting, f'font_px = {32.0 / screen_scale:.6f}'))
    else:
        command('heartbeat')
        event(lambda value: value.get('event') == 'heartbeat-complete', 'finish the real operation before stopping the idle application')
        os.kill(first_pid, signal.SIGSTOP)

    def revoked():
        status = window_status()
        if not status:
            return None
        pane = status['current_panes'][0]
        native = pane.get('native')
        if pane['display_source'] == 'classic' and (native is None or native['installed_frame'] is None and native['generation'] > active['epoch']):
            return status
        return None
    revoked_status = await_file(revoked, 'application loses its overlay with retained shell FD')
    if args.lifecycle not in ('metrics', 'metrics-live'):
        stopped_state = subprocess.check_output(['/bin/ps', '-p', str(first_pid), '-o', 'stat='], text=True).strip()
        assert stopped_state.startswith('T'), ('foreground app did not stop', stopped_state)
    if args.lifecycle == 'lease':
        assert revoked_status['current_panes'][0]['native']['generation'] == active['epoch'] + 1, 'lease expiry must be the only revocation before either process resumes'
    (out / 'stopped-classic-status.json').write_text(json.dumps(revoked_status, indent=2))
    if args.lifecycle == 'ctrl-z':
        os.kill(shell_pid, signal.SIGUSR1)  # The private interactive shell runs its real fg builtin.
        event(lambda value: value.get('event') == 'resumed', 'foreground application resumed by fg')
    elif args.lifecycle in ('lease', 'metrics'):
        os.kill(first_pid, signal.SIGCONT)
    fallback = event(lambda value: value.get('event') == 'fallback', 'resumed stale session uses ANSI', allow_fallback=True)
    assert fallback['error'] == 'native session ended', fallback
    if args.lifecycle in ('metrics', 'metrics-live'):
        assert fallback['ended_reason'] == 1, ('must be real resize revocation', fallback)
    elif args.lifecycle == 'lease':
        assert fallback['ended_reason'] == 3, fallback
    assert revoked_status['current_panes'][0]['native'] and revoked_status['current_panes'][0]['native']['available'], revoked_status['current_panes'][0]
    if args.lifecycle in ('metrics', 'metrics-live'):
        width, height = revoked_status['visible_pane_content_rects'][0][1][2:]
        assert width % fallback['columns'] == 0 and height % fallback['rows'] == 0
        physical_cell[:] = [width // fallback['columns'], height // fallback['rows']]
        assert physical_cell != active['cell'], (active, fallback, physical_cell)
    png, meta, _ = await_file(lambda: presented_frame(None, 1, (1, 1)), 'real resumed ANSI widget fallback')
    (out / 'resumed-ansi.png').write_bytes(png)
    result['cases'].append({'name': args.lifecycle + ' revocation and resumed ANSI', 'present': meta})
    control.close()
    control = None
    completed_job = runtime / ('fg.done' if args.lifecycle == 'ctrl-z' else 'first.done')
    await_file(completed_job.exists, 'interactive shell completed its foreground wait')
    if args.lifecycle == 'ctrl-z':
        os.kill(shell_pid, signal.SIGUSR2)  # Explicitly launch a fresh descendant, retaining the launch FD.
    else:
        trigger = os.open(runtime / 'next.fifo', os.O_WRONLY | os.O_NONBLOCK | os.O_CLOEXEC)
        try:
            assert os.write(trigger, b'next\n') == 5
        finally:
            os.close(trigger)
    control, _ = listener.accept()
    buffer.clear()
    second = event(lambda value: value.get('event') == 'probe-ready', 'fresh descendant starts')
    assert second['pid'] != first_pid
    command('discover')
    fresh_active = event(lambda value: value.get('event') == 'active', 'fresh explicit discovery activates')
    assert fresh_active['frame'] == 1 and fresh_active['epoch'] > active['epoch']
    if args.lifecycle in ('metrics', 'metrics-live'):
        assert fresh_active['columns'] > active['columns'] and fresh_active['rows'] > active['rows'] and fresh_active['cell'] != active['cell'], (active, fresh_active)
        assert physical_cell == fresh_active['cell'], (physical_cell, fresh_active)
    fresh_ready = event(lambda value: value.get('event') == 'frame' and value.get('placements') == 1, 'fresh descendant image ready')
    png, meta, _ = await_file(lambda: presented_frame(fresh_ready['frame'], 1, (1, 1)), 'fresh descendant exact presented image')
    (out / 'fresh-descendant-native.png').write_bytes(png)
    current_owner = window_status()['current_panes'][0]['native']
    assert (current_owner['child_pid'], current_owner['child_birth']) == root_identity
    result['cases'].append({'name': 'fresh descendant on same launch endpoint', 'present': meta, 'epoch': fresh_active['epoch'], 'activation_frame': fresh_active['frame']})
    command('release')
    event(lambda value: value.get('event') == 'released', 'fresh descendant releases')
    result['status'] = 'PASS'

def run_budgets(ready):
    source = out / 'opaque-64m.rgba'
    source.write_bytes(bytes((255,)) * 67108864)
    tiny = out / 'opaque-tiny.rgba'
    tiny.write_bytes(bytes((255,)) * 4)
    over = out / 'opaque-over.rgba'
    shutil.copyfile(source, over)
    with over.open('ab') as stream:
        stream.write(bytes((255,)) * 4)

    def reference(name, count):
        path = out / (name + '.iterm2')
        command('reference ' + str(path))
        observed = event(lambda value: value.get('event') == 'reference', name + ' real PixelLayer result')
        assert len(observed['requests']) == count and all(item['accepted'] for item in observed['requests'])
        assert observed['fallback_cells'] == 0
        encoded = path.read_bytes()
        images = re.findall(rb'\x1b\]1337;File=[^:]*:([A-Za-z0-9+/=]+)(?:\x07|\x1b\\)', encoded)
        assert len(images) == count, (name, len(images))
        for index, payload in enumerate(images):
            decoded = base64.b64decode(payload, validate=True)
            image = png_module.PNG(decoded)
            assert (image.width, image.height) == (176, 196)
            assert all(set(row) == {255} for row in image.rows), 'immediate backend pixels differ from literal white native pixels'
            (out / f'{name}-{index}.png').write_bytes(decoded)
        return observed

    for image_id, width, height, left, path, status in [
        (1, 16777216, 1, 1, source, 0),
        (2, 1, 16777216, 10, source, 0),
        (3, 1, 1, 19, tiny, 2),
    ]:
        command(f'load {image_id} 1 {width} {height} {left} 1 {path}')
        admission = event(lambda value: value.get('event') == 'admission' and value.get('image') == image_id and value.get('generation') == 1, 'real host image admission')
        assert admission['status'] == status, admission
        assert (admission['handle'] != 0) == (status == 0), admission
        assert admission['source_bytes'] == (67108864 if image_id < 3 else 4)
        expected = image_id if image_id < 3 else 2
        accepted = status == 0
        changed = event(lambda value: value.get('event') == 'frame' and value.get('placements') == expected and any(item['image'] == image_id and item['generation'] == 1 and item['accepted'] == accepted for item in value.get('requests', [])), 'real ImageSink submit and fallback')
        assert changed['fallback_cells'] == (32 if image_id == 3 else 0), changed
        positions = [(1, 1), (10, 1), (19, 1)][:image_id]
        png, meta, _ = await_file(lambda: white_image_frame(changed['frame'], expected, positions), 'native and actual fallback presented white pixels')
        (out / f'budget-{image_id}.png').write_bytes(png)
        comparison = reference(f'reference-{image_id}', image_id)
        result['cases'].append({'name': '64MiB source' if image_id < 3 else '128MiB live-source budget decline', 'admission': admission, 'native': changed, 'reference': comparison, 'present': meta})
        ready = changed
    command('move 1 1')
    repeated = event(lambda value: value.get('event') == 'frame' and value.get('frame', 0) > ready['frame'], 'repeat a declined generation')
    assert repeated['placements'] == 2 and repeated['fallback_cells'] == 32
    command('heartbeat')
    event(lambda value: value.get('event') == 'heartbeat-complete', 'budget decline preserved the session')
    assert sum(value.get('event') == 'admission' and value.get('image') == 3 and value.get('generation') == 1 for value in events) == 1
    command(f'load 4 1 16777217 1 28 1 {over}')
    rejected = event(lambda value: value.get('event') == 'source-rejected' and value.get('image') == 4, 'real ImageSource constructor limit')
    assert '67108864' in rejected['error'], rejected
    assert not any(value.get('event') == 'admission' and value.get('image') == 4 for value in events)
    command('heartbeat')
    event(lambda value: value.get('event') == 'heartbeat-complete', 'source rejection preserved the session')
    result['cases'].append({'name': 'constructor rejects 64MiB plus one pixel', 'result': rejected})
    command('release')
    event(lambda value: value.get('event') == 'released', 'budget session releases')
    result['status'] = 'PASS'

def run_animation():
    frames = {}
    colors = {}
    window = window_status()
    rect = window['visible_pane_content_rects'][0][1]
    instance = json.loads((out / 'present.json').read_bytes())['host_instance']
    for generation in range(1, 151):
        color = tuple((generation * factor) % 251 + 1 for factor in (73, 47, 131))
        colors[generation] = color
        path = out / f'animation-{generation}.rgba'
        path.write_bytes(b''.join(bytes((*(color if (x // 2 + y // 2) % 2 == 0 else (0, 0, 0)), 255)) for y in range(64) for x in range(64)))
    captures = {}
    records = []
    with (out / 'animation-pacer.log').open('w') as log:
        pacer = subprocess.Popen([sys.executable, str(root / '.cursor/skills/verify-prismattyc/scripts/native-animation-pacer.py'),
                                  '--fd', str(control.fileno()), '--images', str(out), '--out', str(out / 'animation-pacer.json')],
                                 pass_fds=(control.fileno(),), stdout=subprocess.PIPE, stderr=log)
        try:
            assert select.select([pacer.stdout], [], [], 10)[0], 'animation pacer startup'
            started = json.loads(pacer.stdout.readline())
            assert started['event'] == 'pacing-start'
            start_ns = started['start_ns']
            start = start_ns / 1_000_000_000
            deadline = start + 5.3
            while True:
                while b'\n' in buffer:
                    line, _, remaining = buffer.partition(b'\n')
                    buffer[:] = remaining
                    value = json.loads(line)
                    events.append(value)
                    assert value.get('event') != 'fallback', value
                    if value.get('event') == 'admission':
                        assert value['status'] in (0, 1), value
                    if value.get('event') == 'frame' and value['placements'] == 1:
                        accepted = [item for item in value['requests'] if item['accepted'] and item['image'] == 1]
                        assert len(accepted) == 1
                        frames[value['frame']] = accepted[0]['generation']
                try:
                    before = (out / 'present.json').read_bytes()
                    meta = json.loads(before)
                    if meta['seq'] not in captures:
                        png = (out / 'present.png').read_bytes()
                        if before == (out / 'present.json').read_bytes():
                            assert meta['host_pid'] == host.pid and meta['host_instance'] == instance
                            elapsed = time.monotonic() - start
                            captures[meta['seq']] = (png, elapsed, meta)
                            records.append({'event': 'captured', 'elapsed': elapsed, 'capture_seq': meta['seq']})
                except (FileNotFoundError, json.JSONDecodeError):
                    pass
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    break
                readable, _, _ = select.select([control, kqueue.fileno()], [], [], remaining)
                if control in readable:
                    chunk = control.recv(8192)
                    assert chunk, 'animation client disconnected'
                    buffer.extend(chunk)
                if kqueue.fileno() in readable:
                    kqueue.control(None, 4, 0)
                health()
            assert pacer.poll() is not None, 'pacer did not complete inside the original workload window'
        finally:
            if pacer.poll() is None:
                pacer.terminate()
            pacer.wait(timeout=2)
            pacer.stdout.close()
    pacing = json.loads((out / 'animation-pacer.json').read_text())
    records.extend({'event': 'sent', 'generation': row['generation'], 'elapsed': (row['sent_ns'] - start_ns) / 1_000_000_000,
                    'lateness_ns': row['before_send_ns'] - row['scheduled_ns']} for row in pacing['records'])
    admissions = {value['generation']: value for value in events if value.get('event') == 'admission'
                  and value['image'] == 1 and 1 <= value['generation'] <= 150 and value['monotonic_ns'] >= start_ns}
    cutoff_ns = start_ns + 5_000_000_000
    admitted_in_window = {generation for generation, value in admissions.items() if value['monotonic_ns'] < cutoff_ns}
    ready_in_window = {request['generation'] for value in events if value.get('event') == 'frame'
                       and start_ns <= value['monotonic_ns'] < cutoff_ns for request in value['requests']
                       if request['image'] == 1 and request['accepted'] and request['generation'] in admissions}
    shown = {}
    for capture_seq, (png, elapsed, meta) in captures.items():
        image = png_module.PNG(png)
        x0, y0 = rect[0] + 22, rect[1] + 49
        anchor = image.pixel(x0 + 6, y0)
        generation = next((generation for generation, color in colors.items() if color == anchor), None)
        if generation is None or generation in shown:
            continue
        # Fixed literal Cover crop [3,0,57,64] into [176,196]. Cell fallback
        # cannot retain these two-source-pixel checker edges across the region.
        correct = all(image.pixel(x0 + x, y0 + y) == (colors[generation] if ((3 + x * 57 // 176) // 2 + (y * 64 // 196) // 2) % 2 == 0 else (0, 0, 0)) for y in range(196) for x in range(176))
        if not correct:
            continue
        shown[generation] = elapsed
        (out / f'animation-presented-{generation}.png').write_bytes(png)
        records.append({'event': 'verified', 'generation': generation, 'elapsed': elapsed, 'capture_seq': capture_seq})
    span = max(shown.values()) - min(shown.values()) if shown else None
    counts = {'generated': pacing['generated'], 'admitted': len(admissions), 'ready': len(set(frames.values()) & set(admissions)),
              'presented': len(shown), 'admitted_in_five_seconds': len(admitted_in_window),
              'ready_in_five_seconds': len(ready_in_window), 'admitted_per_second': len(admitted_in_window) / 5,
              'ready_per_second': len(ready_in_window) / 5, 'presented_span_seconds': span,
              'missing_presentations': sorted(set(admissions) - set(shown)),
              'unadmitted_presentations': sorted(set(shown) - set(admissions)),
              'generator_missed_intervals': pacing.get('missed_intervals')}
    result['animation'] = counts
    (out / 'animation-records.json').write_text(json.dumps(records, indent=2))
    (out / 'animation-counts.json').write_text(json.dumps(counts, indent=2))
    assert pacer.returncode == 0 and pacing['status'] == 'PASS', ('invalid workload pacing', pacing)
    assert set(shown) == set(admissions), ('host presentation differs from uploaded generations', counts)
    assert counts['admitted_per_second'] >= 30 and counts['ready_per_second'] >= 30, ('native animation throughput below 30Hz for five seconds', counts)
    assert set(shown) == set(colors), ('animation missed native checker generations', counts)
    assert span <= 5.05, ('animation rate below 30Hz workload', counts)
    final_frame = max(frame for frame, generation in frames.items() if generation == 150)
    def final_present():
        current = window_status()
        if not current:
            return None
        pane = current['current_panes'][0]
        installed = (pane.get('native') or {}).get('installed_frame')
        return current if installed and installed['sequence'] == final_frame and installed['placements'] == 1 and current['last_raster']['present_succeeded'] else None
    final = await_file(final_present, 'last animation generation presented as native')
    assert final['last_raster']['present']['backend'] == args.presenter
    (out / 'animation-final-status.json').write_text(json.dumps(final, indent=2))
    command('release')
    event(lambda value: value.get('event') == 'released', 'animation session releases')
    result['cases'].append({'name': '150 distinct 64x64 checker generations at 30Hz', **counts, 'literal_pixels_per_generation': 34496})
    result['status'] = 'PASS'


def process_command(pid):
    completed = subprocess.run(['/bin/ps', '-ww', '-p', str(pid), '-o', 'command='], capture_output=True, text=True, timeout=5)
    return completed.stdout.strip()

def process_parent(pid):
    completed = subprocess.run(['/bin/ps', '-p', str(pid), '-o', 'ppid='], capture_output=True, text=True, timeout=5)
    return int(completed.stdout.strip()) if completed.stdout.strip() else None

def await_process_exit(pid, bound=2):
    watcher = select.kqueue()
    try:
        try:
            watcher.control([select.kevent(pid, filter=select.KQ_FILTER_PROC, flags=select.KQ_EV_ADD | select.KQ_EV_ONESHOT, fflags=select.KQ_NOTE_EXIT)], 0, 0)
        except ProcessLookupError:
            return True
        return bool(watcher.control(None, 1, bound))
    finally:
        watcher.close()

def cleanup_signal(pid, number):
    try:
        os.kill(pid, number)
    except ProcessLookupError:
        pass

def cleanup_owned_children():
    global shell_pid
    retained = args.lifecycle in ('ctrl-z', 'lease', 'metrics', 'metrics-live')
    if retained and shell_pid is None:
        status = window_status()
        if status:
            shell_pid = status['current_panes'][0]['local_child_pid']
    if retained and shell_pid and host and process_parent(shell_pid) == host.pid:
        cleanup_signal(shell_pid, signal.SIGCONT)
    errors = []
    for pid in owned_probes:
        try:
            if process_command(pid) == ' '.join([str(probe), producer_mode, str(runtime / 'probe.sock')]):
                cleanup_signal(pid, signal.SIGCONT)
                if not await_process_exit(pid):
                    cleanup_signal(pid, signal.SIGKILL)
                    assert await_process_exit(pid), 'owned foreground probe did not exit'
        except BaseException as error:
            errors.append(repr(error))
    try:
        if retained and shell_pid and host and process_parent(shell_pid) == host.pid:
            cleanup_signal(shell_pid, signal.SIGKILL)
            assert await_process_exit(shell_pid), 'owned retained shell did not exit'
    except BaseException as error:
        errors.append(repr(error))
    assert not errors, errors

signal.signal(signal.SIGTERM, lambda *_: (_ for _ in ()).throw(SystemExit(143)))

try:
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    listener.bind(str(runtime / 'probe.sock'))
    os.chmod(runtime / 'probe.sock', 0o600)
    listener.listen(1)
    listener.settimeout(20)
    with (out / 'daemon.log').open('w') as log:
        daemon = subprocess.Popen([str(bins / 'pmuxd'), '--socket', env['PMUX_SOCKET'], '--', '/bin/bash', '--noprofile', '--norc'],
                                  env=env, stdout=log, stderr=log, start_new_session=True)
    await_file(lambda: Path(env['PMUX_SOCKET']).is_socket(), 'daemon socket')
    launch = [str(probe), producer_mode, str(runtime / 'probe.sock')]
    if args.lifecycle == 'ctrl-z':
        assert args.native == 'on'
        launch_text = shlex.join(launch)
        jobs_command = 'jobs -l > ' + shlex.quote(str(runtime / 'jobs.txt'))
        foreground_command = 'fg %1; : > ' + shlex.quote(str(runtime / 'fg.done'))
        initial_command = launch_text + '; : > ' + shlex.quote(str(runtime / 'first.done'))
        env.update(PS1='native-job> ', LC_ALL='C', PROMPT_COMMAND=f'trap {shlex.quote(jobs_command)} CHLD; trap {shlex.quote(foreground_command)} USR1; trap {shlex.quote(launch_text)} USR2; unset PROMPT_COMMAND; {initial_command}')
        launch = ['/bin/bash', '--noprofile', '--norc', '-i']
    elif args.lifecycle in ('lease', 'metrics', 'metrics-live'):
        assert args.native == 'on'
        os.mkfifo(runtime / 'next.fifo', 0o600)
        launcher = out / 'retained-shell.sh'
        launcher.write_text('set +m\nexec 4<> ' + shlex.quote(str(runtime / 'next.fifo')) + '\n'
                            + shlex.join(launch) + ' 4<&-\n'
                            + ': > ' + shlex.quote(str(runtime / 'first.done')) + '\n'
                            + 'IFS= read -r -u 4 request\nexec 4<&-\n'
                            + shlex.join(launch) + '\n')
        launch = ['/bin/bash', '--noprofile', '--norc', str(launcher)]
    with (out / 'host.log').open('w') as log:
        host = subprocess.Popen([str(bins / 'prismattyc-host'), '--no-splash', *(['--experimental-rich'] if args.native == 'on' else []), '--',
                                 *launch],
                                env=env, stdout=log, stderr=log, start_new_session=True)
    control, _ = listener.accept()
    if args.native == 'off':
        fallback = event(lambda value: value.get('event') == 'fallback', 'flag-off ANSI fallback', allow_fallback=True)
        assert fallback['error'] == 'environment variable not found', fallback
        png, meta, counts = await_file(lambda: presented_frame(None, 1, (1, 1)), 'flag-off classic source and actual widget fallback pixels')
        (out / 'classic-fallback.png').write_bytes(png)
        result['cases'].append({'name': 'flag-off ANSI', 'present': meta, 'red_pixels': counts[bytes((255, 0, 0, 255))]})
        result['status'] = 'PASS'
    else:
        event(lambda value: value.get('event') == 'probe-ready', 'probe startup')
        await_file(lambda: (frame if (frame := current_frame()) and frame[2][bytes((255, 0, 0, 255))] >= 100 else None), 'initial image widget ANSI presentation')
        command('discover')
        active = event(lambda value: value.get('event') == 'active', 'native ACTIVE')
        assert active['cell'] == [22, 49], 'this fixed pixel vector requires the configured physical cell measurement'
        ready = event(lambda value: value.get('event') == 'frame' and value.get('placements') == 1, 'native image READY frame')
        print(json.dumps({'active': active, 'ready': ready}), flush=True)
        png, meta, counts = await_file(lambda: presented_frame(ready['frame'], 1, (1, 1)), 'native READY source and exact four-quadrant pixels')
        (out / 'native-ready.png').write_bytes(png)
        (out / 'native-ready.json').write_text(json.dumps(meta, indent=2))
        result['cases'].append({'name': 'ready', 'frame': ready['frame'], 'present': meta, 'red_pixels': counts[bytes((255, 0, 0, 255))]})
        if args.lifecycle == 'animation':
            run_animation()
        elif args.lifecycle == 'text':
            run_text()
        elif args.lifecycle == 'budgets':
            run_budgets(ready)
        elif args.lifecycle != 'images':
            run_lifecycle(active, ready)
        else:
            for text, name, ink, opacity in [
                ('ink on', 'positive image covers real glyph ink', False, 255),
                ('depth -1', 'negative image preserves real glyph ink', True, 255),
                ('ink off', 'negative image with blank cells', False, 255),
                ('state disabled', 'semantic opacity 153', False, 153),
                ('state normal', 'semantic opacity restored', False, 255),
            ]:
                command(text)
                changed = event(lambda value: value.get('event') == 'frame' and value.get('frame', 0) > ready['frame'], name)
                png, meta, counts = await_file(lambda: presented_frame(changed['frame'], 1, (1, 1), ink=ink, opacity=opacity), name)
                (out / (name.replace(' ', '-') + '.png')).write_bytes(png)
                result['cases'].append({'name': name, 'frame': changed['frame'], 'present': meta})
                ready = changed
            command('move 10 6')
            moved = event(lambda value: value.get('event') == 'frame' and value.get('frame', 0) > ready['frame'], 'moved frame')
            png, meta, counts = await_file(lambda: presented_frame(moved['frame'], 1, (10, 6), (1, 1)), 'moved presentation and cleared old rectangle')
            (out / 'native-moved.png').write_bytes(png)
            result['cases'].append({'name': 'moved', 'frame': moved['frame'], 'present': meta})
            command('omit')
            omitted = event(lambda value: value.get('event') == 'frame' and value.get('placements') == 0, 'omission frame')
            png, meta, counts = await_file(lambda: presented_frame(omitted['frame'], 0, (10, 6)), 'omission presentation')
            (out / 'native-omitted.png').write_bytes(png)
            result['cases'].append({'name': 'omitted', 'frame': omitted['frame'], 'present': meta})
            event(lambda value: value.get('event') == 'retired', 'resource retired')
            command('release')
            event(lambda value: value.get('event') == 'released', 'session released')
            result['status'] = 'PASS'

except BaseException as error:
    result['error'] = repr(error)
    raise
finally:
    cleanup_errors = []
    def attempt(label, operation):
        try:
            operation()
        except BaseException as error:
            cleanup_errors.append({'operation': label, 'error': repr(error)})
    def diagnostics():
        for diagnostic in [*runtime.glob('*.txt'), *runtime.glob('*.json')]:
            (out / diagnostic.name).write_bytes(diagnostic.read_bytes())
        if owned_probes:
            observed = subprocess.run(['/bin/ps', '-p', ','.join(map(str, owned_probes)), '-o', 'pid=,ppid=,pgid=,stat=,command='], capture_output=True, text=True, timeout=5)
            (out / 'foreground-processes.txt').write_text(observed.stdout + observed.stderr)
        if host is not None and host.poll() is None:
            if result['status'] != 'PASS':
                subprocess.run(['/usr/bin/sample', str(host.pid), '1', '1', '-file', str(out / 'host-sample.txt')], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=8)
            observed = subprocess.run(['/usr/sbin/lsof', '-a', '-p', str(host.pid), '-d', '0,1,2', '-Fn'], capture_output=True, text=True, timeout=5)
            (out / 'host-stdio.txt').write_text(observed.stdout + observed.stderr)
    def stop(process):
        if process is not None and process.poll() is None:
            try:
                os.killpg(process.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait(timeout=5)
    attempt('diagnostics', diagnostics)
    if control:
        attempt('control socket', control.close)
    if listener:
        attempt('listener socket', listener.close)
    attempt('foreground children', cleanup_owned_children)
    attempt('host process', lambda: stop(host))
    attempt('daemon process', lambda: stop(daemon))
    tracked = owned_probes | ({shell_pid} if shell_pid else set())
    result['host_reaped'] = host is None or host.poll() is not None
    result['daemon_reaped'] = daemon is None or daemon.poll() is not None
    result['remaining_owned_processes'] = [pid for pid in tracked if process_command(pid)]
    if result['remaining_owned_processes']:
        cleanup_errors.append({'operation': 'process audit', 'remaining': result['remaining_owned_processes']})
    attempt('event queue', kqueue.close)
    for descriptor in directory_fds:
        attempt('directory descriptor', lambda descriptor=descriptor: os.close(descriptor))
    attempt('runtime directory', lambda: shutil.rmtree(runtime))
    result['runtime_removed'] = not runtime.exists()
    result['events'] = events
    if cleanup_errors:
        result.update(status='FAIL', cleanup_errors=cleanup_errors)
    (out / 'result.json').write_text(json.dumps(result, indent=2))
    print(json.dumps(result), flush=True)
    assert not cleanup_errors, 'cleanup failed; inspect the retained result receipt'
