#!/usr/bin/env python3
"""GH #345: rapid chip clicks through a delayed wrapper around the real pmux."""
import contextlib
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import time


WRAPPER = r'''#!/usr/bin/env python3
import json, os, subprocess, sys, time
from pathlib import Path
args = sys.argv[1:]
if args[:2] != ['space', 'open']:
    os.execv(os.environ['RACE_REAL_PMUX'], [os.environ['RACE_REAL_PMUX'], *args])
def record(event):
    with open(os.environ['RACE_ORDER'], 'a') as f:
        f.write(json.dumps({'event': event, 'name': args[2], 'at': time.monotonic()}) + '\n')
record('start')
if args[2] in ('race-f', 'race-t'):
    record('end')
    sys.exit(23 if args[2] == 'race-f' else 0)
if args[2] == 'race-a':
    time.sleep(5)
status = subprocess.run([os.environ['RACE_REAL_PMUX'], *args, '--no-run']).returncode
if args[2] == 'race-u':
    # Inject disappearance after the target view is applied. Distinct Space
    # owners require a real regroup before this post-apply failure case.
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        probe = subprocess.run([os.environ['RACE_REAL_PMUX'], 'render-status', '--json'], capture_output=True, text=True)
        if probe.returncode == 0 and any(window.get('space') == 'race-u' for window in json.loads(probe.stdout).get('windows', [])):
            break
        time.sleep(.05)
    else:
        raise RuntimeError('race-u view was not applied before disappearance injection')
    subprocess.run([os.environ['RACE_REAL_PMUX'], 'stop', 'race-u-1'], check=True)
time.sleep(.3)
record('end')
sys.exit(status)
'''


@contextlib.contextmanager
def private_daemon(ux, out):
    with (out / 'pmuxd.log').open('w') as log:
        process = subprocess.Popen(['pmuxd', '--socket', os.environ['PMUX_SOCKET'], '--', '/bin/sh'],
                                   stdout=log, stderr=subprocess.STDOUT)
        try:
            ux.wait_for(lambda: Path(os.environ['PMUX_SOCKET']).is_socket() and ux.run('pmux', 'ls'), 'private mux startup')
            yield
        finally:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()


def split_live_session(name):
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
        client.settimeout(5)
        client.connect(os.environ['PMUX_SOCKET'])
        reader = client.makefile('r')
        sequence = 0
        def call(kind, **fields):
            nonlocal sequence
            sequence += 1
            client.sendall((json.dumps(dict(type=kind, version=1, request_id=sequence, **fields)) + '\n').encode())
            response = json.loads(reader.readline())
            assert response['status'] == 'ok', response
            return response['response']
        call('register_client')
        snapshot = call('snapshot')['snapshot']
        session = next(row for row in snapshot['sessions'] if row['name'] == name)
        window = session['windows'][0]
        call('split', window_id=window['id'], target_pane_id=window['panes'][0]['id'],
             axis='horizontal', ratio=.5, spawn={'program': 'bash', 'argv': ['--noprofile', '--norc'], 'cwd': None, 'env': {}}, client_id=None)


def main():
    out = Path(os.environ.get('HOST_UX_OUT', '/tmp/host-ux-e2e')) / 'space-open-race'
    out.mkdir(parents=True, exist_ok=False)
    real_pmux = shutil.which('pmux')
    assert real_pmux
    # Every daemon, saved Space, PID file, and cache belongs to this fixture.
    for key in list(os.environ):
        if key.startswith(('PMUX_', 'PRISMATTYC_')) or key == 'PMUX':
            del os.environ[key]
    for kind in ['config', 'data', 'state', 'runtime']:
        path = out / kind
        path.mkdir(mode=0o700)
        os.environ[f'XDG_{kind.upper()}_HOME' if kind != 'runtime' else 'XDG_RUNTIME_DIR'] = str(path)
    os.environ['PMUX_SOCKET'] = str(out / 'runtime/pmux.sock')
    os.environ['HOST_UX_OUT'] = str(out)
    spec = importlib.util.spec_from_file_location('host_ux', Path(__file__).with_name('host-ux-e2e.py'))
    ux = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(ux)
    with private_daemon(ux, out):
        spaces = out / 'data/prismattyc/spaces'
        names = ['race-a', 'race-b', 'race-c']
        for count, name in enumerate(names, 1):
            sessions = [f'{name}-{i}' for i in range(count)]
            for session in sessions:
                ux.run(real_pmux, 'new', session, '--no-attach', '--no-agent', '--',
                       'bash', '--noprofile', '--norc')
            if name == 'race-c':
                split_live_session(sessions[-1])
            ux.run(real_pmux, 'space', 'save', name, *sessions)
            path = spaces / f'{name}.json'
            saved = json.loads(path.read_text())
            assert saved['version'] == 2 and saved['id']
            saved.update(tabs=[{'title': session, 'sessions': [session]} for session in sessions],
                         active_tab=count - 1, focused_session=sessions[-1])
            path.write_text(json.dumps(saved))
            if name == 'race-c':
                split_live_session(sessions[-1])
        # Each Space owns distinct sessions. Failed/timeout helpers still have
        # valid saved definitions; the unavailable case removes its own seat.
        for extra in ['race-f', 'race-t', 'race-u']:
            sessions = [f'{extra}-{i}' for i in range(3 if extra == 'race-u' else 1)]
            for session in sessions:
                ux.run(real_pmux, 'new', session, '--no-attach', '--no-agent', '--',
                       'bash', '--noprofile', '--norc')
            ux.run(real_pmux, 'space', 'save', extra, *sessions)
        originals = {p.name: p.read_bytes() for p in spaces.glob('*.json')}
        wrapper = out / 'pmux'
        wrapper.write_text(WRAPPER)
        wrapper.chmod(0o700)
        os.environ.update(PMUX=str(wrapper), RACE_REAL_PMUX=real_pmux, RACE_ORDER=str(out / 'order.jsonl'))
        config = ('font_px = 16.0\nspace_rail = "bottom"\nwindow_padding_px = 5\npane_gap_px = 5\n'
                  'bell_toaster_ms = 4000\nwindow_opacity = 1.0\nspace_rail_pane_names = false\n')
        host = ux.Host('host', ['--', 'bash', '--noprofile', '--norc'], config)
        result = {'host_binary_sha256': hashlib.sha256(Path(shutil.which('prismattyc-host')).read_bytes()).hexdigest()}
        try:
            time.sleep(2)
            host.capture('before')
            geometry = dict(line.split('=', 1) for line in ux.run('xdotool', 'getwindowgeometry', '--shell', host.wid).splitlines())
            def click_space(name):
                chip = next(row for row in host.status()['space_chips'] if row['name'] == name)
                x = int(geometry['X']) + chip['x'] + chip['width'] // 3
                y = int(geometry['Y']) + chip['y'] + chip['height'] // 2
                ux.run('xdotool', 'mousemove', '--sync', str(x), str(y), 'click', '1')
            for name in names:
                click_space(name)
                time.sleep(.08)
            # No corrective input follows the three clicks. Observe the idle host.
            time.sleep(2)
            pending = host.status()
            assert pending['space_open_pending'] and pending['space'] is None, pending
            host.capture('pending')
            def finished():
                state = host.status()
                return state if state['space'] == 'race-c' and not state['space_open_pending'] else None
            ux.wait_for(finished, 'ordered A/B/C apply', timeout=25)
            host.capture('reuse-toast')
            time.sleep(5)
            state = host.status()
            cache = json.loads((out / 'runtime/pmux.attach-tabs.json').read_text())
            order = [json.loads(line) for line in (out / 'order.jsonl').read_text().splitlines()]
            assert [(row['event'], row['name']) for row in order] == [
                (event, name) for name in names for event in ['start', 'end']], order
            assert cache['space'] == state['space'] == 'race-c'
            assert cache['active_tab'] == state['selected_tab'] == 2
            assert cache['focused_session'] == state['focused_session']
            assert len(cache['tabs']) == len(state['current_panes']) == 3, state
            assert state['pane_count'] == 1, 'each tab contains one active pane'
            assert '3 tabs' in ux.run('xdotool', 'getwindowname', host.wid)
            assert originals == {p.name: p.read_bytes() for p in spaces.glob('*.json')}, 'open changed saved Space definitions'
            host.capture('applied-c')
            host.display('applied-c')
            receipt = state['last_space_open']
            assert receipt['sequence'] == 3 and receipt['mode'] == 'switch', receipt
            reused = next(seat for seat in receipt['seats'] if seat['name'] == 'race-c-2')
            assert receipt['view'] == 'applied' and receipt['launch'] == 'not observed'
            assert reused['state'] == 'reused' and reused['saved_panes'] == 2 and reused['live']['panes'] == 3, receipt
            assert 'live layouts retained' in (host.directory / 'host.log').read_text()
            assert 'bell-toasts' not in state['last_raster']['guards'], 'receipt check must outlive its toast'
            outcomes = {}
            for extra in ['race-f', 'race-t', 'race-u']:
                click_space(extra)
                time.sleep(2)
                def outcome_landed():
                    current = host.status()
                    report = current.get('last_space_open')
                    return current if report and report['name'] == extra and not current['space_open_pending'] else None
                current = ux.wait_for(outcome_landed, extra + ' outcome', timeout=10)
                host.capture(extra + '-toast')
                report = current['last_space_open']
                if extra == 'race-u':
                    assert current['space'] == extra and report['view'] == 'applied'
                    missing = next(seat for seat in report['seats'] if seat['name'] == 'race-u-1')
                    assert missing['state'] == 'unavailable', report
                else:
                    assert current['space'] == 'race-c' and report['view'] == 'not_applied', report
                    assert report['error'] and ('23' in report['error'] if extra == 'race-f' else 'did not apply' in report['error']), report
                outcomes[extra] = report
            result.update(status='PASS', order=order, applied=state, cache=cache, outcomes=outcomes)
            print('SPACE_OPEN_RACE_COMPLETE: A/B/C serialized; applied chip, cache, and focus agree', flush=True)
            print('SPACE_OPEN_OUTCOMES_COMPLETE: retained 3 vs saved 2 panes; helper failure, apply timeout, unavailable seat', flush=True)
        except Exception as error:
            result.update(status='FAIL', error=str(error))
            raise
        finally:
            (out / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
            host.stop()
            # This socket is private; stop only the fixture sessions.
            for count, name in enumerate(names, 1):
                for i in range(count):
                    subprocess.run([real_pmux, 'stop', f'{name}-{i}'], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)



if __name__ == '__main__':
    main()
