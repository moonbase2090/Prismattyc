#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Drive an isolated real X11 host, using filesystem and control-socket signals."""
import argparse
import hashlib
import ctypes
import importlib.util
import json
import os
from pathlib import Path
import select
import shlex
import shutil
import signal
import socket
import subprocess
import tempfile
import time

parser = argparse.ArgumentParser()
parser.add_argument('--out', type=Path, required=True)
parser.add_argument('--bins', type=Path, required=True)
parser.add_argument('--probe', type=Path, required=True)
parser.add_argument('--repo', type=Path, required=True)
parser.add_argument('--mode', choices=('images', 'legacy', 'off', 'resize', 'text', 'text-no-mouse'), required=True)
args = parser.parse_args()
producer_mode = {'legacy': '--legacy-direct-local', 'text': '--direct-local-alt',
                 'text-no-mouse': '--direct-local-alt-no-mouse'}.get(args.mode, '--direct-local')
out = args.out.resolve()
out.mkdir(exist_ok=False)
runtime = Path(tempfile.mkdtemp(prefix='native-linux-', dir='/tmp'))
bins = args.bins.resolve()
probe = args.probe.resolve()
source = args.repo.resolve()
spec = importlib.util.spec_from_file_location('png_decode', source / 'tests/native/host-ux-e2e.py')
png_decode = importlib.util.module_from_spec(spec)
spec.loader.exec_module(png_decode)
spec = importlib.util.spec_from_file_location('native_clipboard', source / '.cursor/skills/verify-prismattyc/scripts/native-x11-clipboard.py')
native_clipboard = importlib.util.module_from_spec(spec)
spec.loader.exec_module(native_clipboard)
libc = ctypes.CDLL(None, use_errno=True)
libc.inotify_init1.argtypes = [ctypes.c_int]
libc.inotify_add_watch.argtypes = [ctypes.c_int, ctypes.c_char_p, ctypes.c_uint32]
notify = libc.inotify_init1(os.O_NONBLOCK | os.O_CLOEXEC)
assert notify >= 0
for directory in (out, runtime):
    assert libc.inotify_add_watch(notify, os.fsencode(directory), 0x18A) >= 0
env = {key: value for key, value in os.environ.items() if not key.startswith(('PMUX', 'PRISMATTYC_', 'XDG_'))}
for startup in ('BASH_ENV', 'ENV', 'PROMPT_COMMAND'):
    env.pop(startup, None)
for directory in ('home', 'config', 'data', 'state'):
    (out / directory).mkdir()
env.update(HOME=str(out / 'home'), PATH=f'{bins}:/usr/local/bin:/usr/bin:/bin', SHELL='/bin/bash',
           PMUX_SOCKET=str(runtime / 'pmux.sock'), XDG_RUNTIME_DIR=str(runtime),
           XDG_CONFIG_HOME=str(out / 'config'), XDG_DATA_HOME=str(out / 'data'), XDG_STATE_HOME=str(out / 'state'),
           PRISMATTYC_CONFIG=str(out / 'config.toml'), PRISMATTYC_DUMP_PRESENT=str(out / 'present.png'),
           PRISMATTYC_E2E_WINDOW_CELLS='64x12', WINIT_UNIX_BACKEND='x11')
(out / 'config.toml').write_text('splash = false\nautomatic_update_checks = false\nstart_at_login = false\nspace_startup = "fresh"\ninstall_agent_skills = false\nfont_px = 18.0\nwindow_opacity = 1.0\nwindow_padding_px = 0\npane_padding_px = 0\nfocus_border_animation = "none"\n')
if args.mode.startswith('text'):
    with (out / 'config.toml').open('a') as configuration:
        configuration.write('smart_ctrl_c = true\n')
(out / 'binary-sha256.json').write_text(json.dumps({str(path.name): hashlib.sha256(path.read_bytes()).hexdigest() for path in [bins / 'prismattyc-host', bins / 'pmuxd', probe]}, indent=2))
host = daemon = server = control = listener = None
probe_pid = None
owned_probes = set()
events = []
buffer = bytearray()
terminal_input = bytearray()
result = {'status': 'FAIL', 'mode': args.mode, 'cases': []}

def health():
    for process in (host, daemon, server):
        if process:
            assert process.poll() is None, f'owned process exited: {process.args}'

def await_file(predicate, label, bound=10):
    deadline = time.monotonic() + bound
    while True:
        health()
        value = predicate()
        if value:
            return value
        remaining = deadline - time.monotonic()
        assert remaining > 0 and select.select([notify], [], [], remaining)[0], label
        os.read(notify, 65536)

def event(predicate, label, allow_fallback=False):
    global probe_pid
    deadline = time.monotonic() + 10
    while True:
        while b'\n' in buffer:
            line, _, remaining = buffer.partition(b'\n')
            buffer[:] = remaining
            value = json.loads(line)
            events.append(value)
            if value['event'] == 'input':
                terminal_input.extend(value['bytes'])
            if value['event'] == 'probe-ready':
                probe_pid = value['pid']
                owned_probes.add(probe_pid)
            assert allow_fallback or value['event'] != 'fallback', value
            if predicate(value):
                return value
        health()
        remaining = deadline - time.monotonic()
        assert remaining > 0 and select.select([control], [], [], remaining)[0], label
        chunk = control.recv(8192)
        assert chunk, label + ': client disconnected'
        buffer.extend(chunk)

def command(text):
    control.sendall(text.encode() + b'\n')

def selected_text_frame(frame, label=None, guard=None, no_footer=False, after_capture=0):
    try:
        status = json.loads((runtime / 'pmux.host.render.json').read_bytes())
        assert status['host_pid'] == host.pid
        window = status['windows'][0]
        pane = window['current_panes'][0]
        installed = (pane.get('native') or {}).get('installed_frame')
        if frame is None:
            if pane['display_source'] != 'classic':
                return None
        elif pane['display_source'] != 'native' or not installed or installed['sequence'] != frame or installed['placements'] != 0:
            return None
        before = (out / 'present.json').read_bytes()
        png = (out / 'present.png').read_bytes()
        if before != (out / 'present.json').read_bytes():
            return None
        meta = json.loads(before)
        if not window['last_raster']['present_succeeded'] or window['last_raster']['capture_seq'] != meta['seq']:
            return None
        if meta['seq'] <= after_capture:
            return None
        if guard and guard not in window['last_raster']['guards']:
            return None
        if no_footer and 'footer-visible' in window['last_raster']['guards']:
            return None
        assert meta['host_instance'] == status['host_instance']
        name = label or pane['display_source']
        (out / f'text-{name}.png').write_bytes(png)
        (out / f'text-{name}-status.json').write_text(json.dumps(status, indent=2))
        return {'source': pane['display_source'], 'frame': frame, 'capture': meta['seq'], 'guards': window['last_raster']['guards']}
    except (FileNotFoundError, json.JSONDecodeError):
        return None

def copy_keys(keys):
    clipboard = native_clipboard.Clipboard(env['DISPLAY'])
    try:
        subprocess.run(['xdotool', 'key', '--clearmodifiers', *keys], env=env, check=True, timeout=10)
        return clipboard.copied_text()
    finally:
        clipboard.close()

def drag(end_column, shift):
    window = json.loads((runtime / 'pmux.host.render.json').read_bytes())['windows'][0]
    _, (left, top, _, _) = window['visible_pane_content_rects'][0]
    cw, ch = active['cell']
    keys = ['xdotool', 'mousemove', '--window', window_ids[0], str(left + cw // 2), str(top + ch // 2)]
    if shift:
        keys += ['keydown', 'Shift_L']
    keys += ['mousedown', '1', 'mousemove', '--window', window_ids[0],
             str(left + end_column * cw + cw // 2), str(top + ch // 2), 'mouseup', '1']
    if shift:
        keys += ['keyup', 'Shift_L']
    subprocess.run(keys, env=env, check=True, timeout=10)

def copy_drag(end_column, shift):
    clipboard = native_clipboard.Clipboard(env['DISPLAY'])
    try:
        drag(end_column, shift)
        return clipboard.copied_text()
    finally:
        clipboard.close()

def text_readers():
    command('text-scene')
    changed = event(lambda value: value['event'] == 'frame' and value['placements'] == 0, 'native text scene')
    native = await_file(lambda: selected_text_frame(changed['frame']), 'native text presented')
    pane = json.loads((runtime / 'pmux.host.render.json').read_bytes())['windows'][0]['current_panes'][0]
    tracking = args.mode == 'text'
    assert pane['alt_active'] and (pane['mouse_tracking'] != 'Off') == tracking, pane
    result['phase'] = 'native-find-over-retained-alt'
    subprocess.run(['xdotool', 'key', '--clearmodifiers', 'Escape', 'ctrl+shift+f'], env=env, check=True, timeout=10)
    opened = await_file(lambda: selected_text_frame(changed['frame'], label='find-open', guard='find-active', no_footer=True), 'native Find opens over the retained alternate screen')
    subprocess.run(['xdotool', 'type', '--clearmodifiers', '--delay', '0', 'N'], env=env, check=True, timeout=10)
    def first_query_frame():
        frame = selected_text_frame(changed['frame'], label='find-first-key', after_capture=opened['capture'])
        if frame:
            assert 'find-active' in frame['guards'], 'native Find closed on the first query key'
        return frame
    result['cases'].append(await_file(first_query_frame, 'native Find presents the first query key'))
    subprocess.run(['xdotool', 'type', '--clearmodifiers', '--delay', '0', 'ATIVE-CLUSTER'], env=env, check=True, timeout=10)
    match = copy_keys(['ctrl+shift+c'])
    assert match == 'NATIVE-CLUSTER', repr(match)
    found = await_file(lambda: selected_text_frame(changed['frame'], label='find', guard='find-active', no_footer=True), 'native find selection presented without the shortcut strip')
    found.update(reader='find', clipboard=match)
    result['cases'].append(found)
    subprocess.run(['xdotool', 'key', '--clearmodifiers', 'Escape'], env=env, check=True, timeout=10)
    native['clipboard'] = copy_keys(['ctrl+shift+a', 'ctrl+shift+c'])
    expected = 'NATIVE-CLUSTER 界e' + '\u0301' * 20 + '\n\nhttps://example.test/native'
    assert native['clipboard'].rstrip('\n') == expected, repr(native['clipboard'])
    result['cases'].append(native)
    subprocess.run(['xdotool', 'key', '--clearmodifiers', 'Escape'], env=env, check=True, timeout=10)
    result['phase'] = 'native-mouse-over-retained-alt'
    if tracking:
        terminal_input.clear()
        clipboard = native_clipboard.Clipboard(env['DISPLAY'])
        try:
            drag(17, False)
            event(lambda value: b'\x1b[<0;18;1m' in terminal_input, 'plain native drag reaches the application')
            assert b'\x1b[<0;1;1M' in terminal_input and b'\x1b[<32;18;1M' in terminal_input, bytes(terminal_input)
            assert clipboard.owns_selection(), 'plain tracked drag unexpectedly copied a host selection'
            result['cases'].append({'reader': 'application mouse', 'input': list(terminal_input), 'host_selection_copied': False})
        finally:
            clipboard.close()
    else:
        copied = copy_drag(17, False)
        assert copied == expected.split('\n')[0], repr(copied)
        result['cases'].append({'reader': 'plain native drag without tracking', 'clipboard': copied})
    if tracking:
        copied = copy_drag(17, True)
        assert copied == expected.split('\n')[0], repr(copied)
        result['cases'].append({'reader': 'Shift native drag', 'clipboard': copied})
    command('release-hold')
    event(lambda value: value['event'] == 'released', 'release exposes retained Classic source')
    classic = await_file(lambda: selected_text_frame(None), 'retained Classic text presented')
    classic['clipboard'] = copy_drag(15, True)
    assert 'CLASSIC-BASELINE' in classic['clipboard'], repr(classic['clipboard'])
    assert 'NATIVE-CLUSTER' not in classic['clipboard'] and 'example.test' not in classic['clipboard']
    result['cases'].append(classic)
    result['phase'] = 'complete'

def presentation(frame, position, placements, old=None):
    try:
        status = json.loads((runtime / 'pmux.host.render.json').read_bytes())
        assert status['host_pid'] == host.pid
        window = status['windows'][0]
        pane = window['current_panes'][0]
        installed = (pane.get('native') or {}).get('installed_frame')
        if frame is None:
            if pane['display_source'] != 'classic':
                return None
        elif pane['display_source'] != 'native' or not installed or installed['sequence'] != frame or installed['placements'] != placements:
            return None
        before = (out / 'present.json').read_bytes()
        png = (out / 'present.png').read_bytes()
        if before != (out / 'present.json').read_bytes():
            return None
        meta = json.loads(before)
        if not window['last_raster']['present_succeeded'] or window['last_raster']['capture_seq'] != meta['seq']:
            return None
        assert meta['host_instance'] == status['host_instance']
        _, (left, top, width, height) = window['visible_pane_content_rects'][0]
        columns, rows = active['columns'], active['rows']
        assert width % columns == 0 and height % rows == 0
        cw, ch = width // columns, height // rows
        image = png_decode.PNG(png)
        colors = ((255, 0, 0), (0, 0, 255), (255, 255, 0), (0, 255, 255))
        for y in range(4 * ch):
            for x in range(8 * cw):
                expected = colors[(2 if y >= 2 * ch else 0) + (1 if x >= 4 * cw else 0)] if position else (0, 0, 0)
                target = position or (1, 1)
                if image.pixel(left + target[0] * cw + x, top + target[1] * ch + y) != expected:
                    return None
                if old and image.pixel(left + old[0] * cw + x, top + old[1] * ch + y) != (0, 0, 0):
                    return None
        (out / f'frame-{frame}-{meta["seq"]}.png').write_bytes(png)
        (out / f'frame-{frame}-{meta["seq"]}-status.json').write_text(json.dumps(status, indent=2))
        return {'meta': meta, 'cell': [cw, ch], 'source': pane['display_source'], 'placements': placements, 'owner': pane.get('native')}
    except (FileNotFoundError, json.JSONDecodeError):
        return None

def stop(process):
    if process and process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)

def resize_and_rearm(initial):
    global control, active
    old_epoch = active['epoch']
    owner = initial['owner']
    assert owner and owner['child_pid'] != probe_pid
    window_ids = subprocess.check_output(['xdotool', 'search', '--sync', '--pid', str(host.pid)], env=env, text=True, timeout=10).split()
    assert len(window_ids) == 1, window_ids
    subprocess.run(['xdotool', 'windowsize', '--sync', window_ids[0], '900', '600'], env=env, check=True, timeout=10)
    fallback = event(lambda value: value['event'] == 'fallback', 'resize ends the old native session', True)
    assert fallback['error'] == 'native session ended', fallback
    assert fallback['columns'] > active['columns'] and fallback['rows'] > active['rows'], (active, fallback)
    active = {'columns': fallback['columns'], 'rows': fallback['rows']}
    recovered = await_file(lambda: presentation(None, (1, 1), 0), 'resized full ANSI widget pixels')
    assert recovered['owner'] and recovered['owner']['available'] and recovered['owner']['installed_frame'] is None
    assert recovered['owner']['generation'] > old_epoch
    result['cases'].append(recovered)
    control.close()
    control = None
    await_file(lambda: (runtime / 'first.done').exists(), 'retained shell completed its foreground wait')
    trigger = os.open(runtime / 'next.fifo', os.O_WRONLY | os.O_NONBLOCK | os.O_CLOEXEC)
    try:
        assert os.write(trigger, b'next\n') == 5
    finally:
        os.close(trigger)
    control, _ = listener.accept()
    buffer.clear()
    event(lambda value: value['event'] == 'probe-ready', 'fresh descendant starts')
    command('discover')
    active = event(lambda value: value['event'] == 'active', 'fresh resized native session')
    assert active['epoch'] > old_epoch and active['frame'] == 1
    event(lambda value: value['event'] == 'frame' and value['placements'] == 1, 'fresh resized image ready')
    command('fit stretch')
    changed = event(lambda value: value['event'] == 'frame' and value['placements'] == 1, 'fresh literal stretch frame')
    fresh = await_file(lambda: presentation(changed['frame'], (1, 1), 1), 'fresh descendant exact image at resized geometry')
    assert (fresh['owner']['child_pid'], fresh['owner']['child_birth']) == (owner['child_pid'], owner['child_birth'])
    result['cases'].append(fresh)
signal.signal(signal.SIGTERM, lambda *_: (_ for _ in ()).throw(SystemExit(143)))
try:
    read_display, write_display = os.pipe()
    try:
        with (out / 'xvfb.log').open('w') as log:
            server = subprocess.Popen(['Xvfb', '-displayfd', str(write_display), '-screen', '0', '1920x1200x24', '-nolisten', 'tcp'], pass_fds=(write_display,), stdout=log, stderr=log)
        os.close(write_display)
        write_display = None
        assert select.select([read_display], [], [], 20)[0], 'Xvfb did not become ready'
        display = os.read(read_display, 128).decode().strip()
        assert display.isdecimal()
        env['DISPLAY'] = ':' + display
    finally:
        os.close(read_display)
        if write_display is not None:
            os.close(write_display)
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    listener.bind(str(runtime / 'probe.sock'))
    os.chmod(runtime / 'probe.sock', 0o600)
    listener.listen(1)
    listener.settimeout(20)
    with (out / 'daemon.log').open('w') as log:
        daemon = subprocess.Popen([str(bins / 'pmuxd'), '--socket', env['PMUX_SOCKET'], '--', '/bin/bash', '--noprofile', '--norc'], env=env, stdout=log, stderr=log)
    await_file(lambda: Path(env['PMUX_SOCKET']).is_socket(), 'private daemon socket')
    launch = [str(probe), producer_mode, str(runtime / 'probe.sock')]
    if args.mode == 'resize':
        os.mkfifo(runtime / 'next.fifo', 0o600)
        launcher = out / 'retained-shell.sh'
        launcher.write_text('set +m\nexec 4<> ' + shlex.quote(str(runtime / 'next.fifo')) + '\n'
                            + shlex.join(launch) + ' 4<&-\n: > ' + shlex.quote(str(runtime / 'first.done'))
                            + '\nIFS= read -r -u 4 request\nexec 4<&-\n' + shlex.join(launch) + '\n')
        launch = ['/bin/bash', '--noprofile', '--norc', str(launcher)]
    with (out / 'host.log').open('w') as log:
        host = subprocess.Popen([str(bins / 'prismattyc-host'), '--no-splash', *([] if args.mode == 'off' else ['--experimental-rich']), '--', *launch], env=env, stdout=log, stderr=log)
    control, _ = listener.accept()
    if args.mode == 'off':
        fallback = event(lambda value: value['event'] == 'fallback', 'disabled native stays ANSI', True)
        assert fallback['error'] == 'environment variable not found', fallback
        active = {'columns': fallback['columns'], 'rows': fallback['rows']}
        result['cases'].append(await_file(lambda: presentation(None, (1, 1), 0), 'disabled actual ANSI widget pixels'))
    else:
        event(lambda value: value['event'] == 'probe-ready', 'probe startup')
        window_ids = subprocess.check_output(['xdotool', 'search', '--onlyvisible', '--sync', '--pid', str(host.pid)], env=env, text=True, timeout=10).split()
        assert len(window_ids) == 1, window_ids
        subprocess.run(['xdotool', 'windowfocus', '--sync', window_ids[0]], env=env, check=True, timeout=10)
        command('discover')
        active = event(lambda value: value['event'] == 'active', 'native activation')
        if args.mode == 'legacy':
            assert active['minor'] == 0 and active['fallback_cells'] == 32
            frame = 1
            placements = 0
        else:
            event(lambda value: value['event'] == 'frame' and value['placements'] == 1, 'image ready')
            command('fit stretch')
            changed = event(lambda value: value['event'] == 'frame' and value['placements'] == 1, 'literal stretch frame')
            frame, placements = changed['frame'], 1
        initial = await_file(lambda: presentation(frame, (1, 1), placements), 'native initial exact pixels')
        result['cases'].append(initial)
        if args.mode.startswith('text'):
            text_readers()
        elif args.mode == 'resize':
            resize_and_rearm(initial)
        else:
            command('move 13 1')
            changed = event(lambda value: value['event'] == 'frame' and value['frame'] > frame, 'move frame')
            frame = changed['frame']
            result['cases'].append(await_file(lambda: presentation(frame, (13, 1), placements, (1, 1)), 'native move erases old image'))
            command('omit')
            changed = event(lambda value: value['event'] == 'frame' and value['frame'] > frame, 'omitting frame')
            frame = changed['frame']
            result['cases'].append(await_file(lambda: presentation(frame, None, 0, (13, 1)), 'native omission erases image'))
            if args.mode == 'images':
                event(lambda value: value['event'] == 'retired' and value['image'] == 1 and value['generation'] == 0, 'omitted image retirement completes')
        if not args.mode.startswith('text'):
            command('release')
            event(lambda value: value['event'] == 'released', 'native release')
    result['status'] = 'PASS'
except BaseException as error:
    result['error'] = repr(error)
    if server and server.poll() is None:
        with (out / 'x11-window-census.log').open('w') as log:
            for diagnostic in (['xwininfo', '-root', '-tree'], ['xprop', '-root']):
                try:
                    subprocess.run(diagnostic, env=env, stdout=log, stderr=log, timeout=5)
                except (OSError, subprocess.TimeoutExpired) as diagnostic_error:
                    result.setdefault('diagnostic_errors', []).append(repr(diagnostic_error))
    raise
finally:
    cleanup_errors = []
    root_pids = set()
    def attempt(label, operation):
        try:
            operation()
        except BaseException as error:
            cleanup_errors.append({'operation': label, 'error': repr(error)})
    def diagnostics():
        for diagnostic in runtime.glob('*.json'):
            (out / diagnostic.name).write_bytes(diagnostic.read_bytes())
        if args.mode == 'resize':
            status_path = runtime / 'pmux.host.render.json'
            if status_path.exists():
                status = json.loads(status_path.read_bytes())
                assert status['host_pid'] == host.pid
                root_pids.add(status['windows'][0]['current_panes'][0]['local_child_pid'])
    def reap(pid, expected):
        try:
            fd = os.pidfd_open(pid)
        except ProcessLookupError:
            return
        try:
            if select.select([fd], [], [], 2)[0]:
                return
            arguments = Path(f'/proc/{pid}/cmdline').read_bytes().split(b'\0')
            assert arguments[:len(expected)] == expected, 'process identity changed before cleanup'
            signal.pidfd_send_signal(fd, signal.SIGCONT)
            signal.pidfd_send_signal(fd, signal.SIGTERM)
            if not select.select([fd], [], [], 2)[0]:
                signal.pidfd_send_signal(fd, signal.SIGKILL)
                assert select.select([fd], [], [], 2)[0], 'owned process did not exit'
        finally:
            os.close(fd)
    attempt('diagnostics', diagnostics)
    if control:
        attempt('control socket', control.close)
    if listener:
        attempt('listener socket', listener.close)
    for pid in owned_probes:
        expected = [os.fsencode(probe), producer_mode.encode(), os.fsencode(runtime / 'probe.sock')]
        attempt('foreground producer', lambda pid=pid, expected=expected: reap(pid, expected))
    for pid in root_pids - owned_probes:
        expected = [b'/bin/bash', b'--noprofile', b'--norc', os.fsencode(out / 'retained-shell.sh')]
        attempt('retained shell', lambda pid=pid, expected=expected: reap(pid, expected))
    for process in (host, daemon, server):
        attempt('owned child process', lambda process=process: stop(process))
    remaining = []
    for pid in owned_probes | root_pids:
        try:
            if Path(f'/proc/{pid}/cmdline').read_bytes():
                remaining.append(pid)
        except FileNotFoundError:
            pass
    if remaining:
        cleanup_errors.append({'operation': 'process audit', 'remaining': remaining})
    result.update(host_reaped=host is None or host.poll() is not None, daemon_reaped=daemon is None or daemon.poll() is not None,
                  xserver_reaped=server is None or server.poll() is not None,
                  remaining_owned_processes=remaining, events=events)
    attempt('runtime directory', lambda: shutil.rmtree(runtime))
    attempt('filesystem notification descriptor', lambda: os.close(notify))
    result['runtime_removed'] = not runtime.exists()
    if cleanup_errors:
        result.update(status='FAIL', cleanup_errors=cleanup_errors)
    (out / 'result.json').write_text(json.dumps(result, indent=2))
    print(json.dumps(result), flush=True)
    assert not cleanup_errors, 'cleanup failed; inspect the retained result receipt'
