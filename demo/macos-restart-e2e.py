#!/usr/bin/env python3
"""Validate Mac presented pixels, live PTYs, and isolated component restarts."""
import subprocess, os, json, socket, tempfile, time, hashlib, shutil
from pathlib import Path
import argparse, importlib.util, sys
parser = argparse.ArgumentParser(description='Check native macOS rendering and component restart with private sessions.')
parser.add_argument('--bins', type=Path, required=True)
parser.add_argument('--out', type=Path, required=True)
args = parser.parse_args()
assert sys.platform == 'darwin', 'Run this fixture on macOS in a logged-in desktop session'
bins = args.bins.resolve()
out = args.out.resolve()
out.mkdir(parents=True, exist_ok=False)
spec = importlib.util.spec_from_file_location('host_ux', Path(__file__).with_name('host-ux-e2e.py'))
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
PNG = module.PNG
records = []
daemon = host = mcp = None
runtime = Path(tempfile.mkdtemp(prefix='pmac-', dir='/tmp'))
env = {k: v for k, v in os.environ.items() if not k.startswith(('PMUX', 'PRISMATTYC_', 'XDG_'))}
home = out / 'home'
home.mkdir()
env.update(HOME=str(home), PATH=str(bins) + ':/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin', SHELL='/bin/bash', PMUX_SOCKET=str(runtime / 'pmux.sock'), XDG_RUNTIME_DIR=str(runtime), XDG_CONFIG_HOME=str(out / 'config'), XDG_DATA_HOME=str(out / 'data'), XDG_STATE_HOME=str(out / 'state'), PRISMATTYC_DUMP_PRESENT=str(out / 'present.png'))
config = out / 'config.toml'
config.write_text('font_px = 16.0\nspace_rail = "top"\nspace_startup = "restore"\nwindow_opacity = 1.0\n[keys]\nblank_split_right = "ctrl+alt+b"\nspace_settings = "ctrl+alt+p"\nupdate_restart = "ctrl+alt+u"\n')
env['PRISMATTYC_CONFIG'] = str(config)

def wait(fn, label, timeout=20):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        try:
            value = fn()
            if value:
                return value
        except (OSError, ValueError, KeyError, subprocess.CalledProcessError):
            pass
        time.sleep(0.1)
    raise AssertionError('timeout: ' + label)

def cli(*args):
    p = subprocess.run([str(bins / 'pmux'), *map(str, args)], env=env, capture_output=True, text=True, timeout=30)
    with (out / 'commands.jsonl').open('a') as f:
        f.write(json.dumps({'args': args, 'exit': p.returncode, 'stdout': p.stdout, 'stderr': p.stderr}) + '\n')
    if p.returncode:
        raise subprocess.CalledProcessError(p.returncode, args, p.stdout, p.stderr)
    return p.stdout
counter = 0

def snapshot():
    global counter
    counter += 1
    with socket.socket(socket.AF_UNIX) as s:
        s.settimeout(5)
        s.connect(env['PMUX_SOCKET'])
        s.sendall((json.dumps({'version': 1, 'request_id': counter, 'type': 'snapshot'}) + '\n').encode())
        return json.loads(s.makefile().readline())['response']['snapshot']

def status():
    data = json.loads(cli('render-status', '--json'))
    assert data['host_pid'] == host.pid
    return data['windows'][0]

def capture(name):

    def ready():
        try:
            a = (out / 'present.json').read_bytes()
            png = (out / 'present.png').read_bytes()
            b = (out / 'present.json').read_bytes()
            return (png, a) if a == b else None
        except FileNotFoundError:
            return None
    png, meta = wait(ready, 'render capture')
    (out / (name + '.png')).write_bytes(png)
    (out / (name + '.json')).write_bytes(meta)
    if name in ('pane-write', 'host-restarted', 'restart-1s') or name.startswith('restart-after-'):
        frame = PNG(png)
        green = sum((row[i:i + 3] == bytes((17, 231, 149)) for row in frame.rows for i in range(0, len(row), frame.bpp)))
        assert green > 200, f'{name}: terminal output lost from presented frame ({green} marker pixels)'
        (out / (name + '-pixels.json')).write_text(json.dumps({'green_pixels': green, 'sha256': hashlib.sha256(png).hexdigest()}))

def record(name, **kw):
    records.append(dict(check=name, **kw))
    (out / 'result.json').write_text(json.dumps({'status': 'RUNNING', 'records': records}, indent=2))
    print('PASS ' + name, flush=True)
try:
    log = (out / 'daemon.log').open('w')
    daemon = subprocess.Popen([str(bins / 'pmuxd'), '--socket', env['PMUX_SOCKET'], '--', '/bin/bash', '--noprofile', '--norc'], env=env, stdout=log, stderr=log)
    wait(lambda: Path(env['PMUX_SOCKET']).exists(), 'private daemon')
    cli('space', 'create', 'mac-alpha', '--no-attach')
    cli('space', 'create', 'mac-beta', '--no-attach')
    sessions = snapshot()['sessions']
    alpha = next((s for s in sessions if s['name'] == 'mac-alpha-1'))
    pane = alpha['windows'][0]['panes'][0]
    identity = (pane['id'], pane['child_pid'])
    log = (out / 'host.log').open('w')
    host = subprocess.Popen([str(bins / 'prismattyc-host'), '--no-splash', '--attach-session', str(alpha['id'])], env=env, stdout=log, stderr=log)
    (out / 'processes.json').write_text(json.dumps({'host_pid': host.pid, 'daemon_pid': daemon.pid, 'runtime': str(runtime)}))
    wait(lambda: status()['space'] == 'mac-alpha', 'native Cocoa window and registered render status')
    capture('initial')
    record('native-window-and-space-open')
    time.sleep(1)
    receipt = json.loads(cli('pane-write', pane['id'], '--text', "printf '\\033[48;2;17;231;149mMAC_%s_OK\\033[0m\\n' NATIVE", '--submit', 'enter', '--json'))
    assert receipt['status'] == 'queued', receipt
    marker = wait(lambda: 'MAC_NATIVE_OK' in cli('attach', 'mac-alpha-1', '--json'), 'pane write visible in terminal')
    time.sleep(0.5)
    capture('pane-write')
    record('intentional-pane-write', receipt=receipt)
    cli('space', 'open', 'mac-beta', '--no-attach', '--no-run')
    wait(lambda: status()['space'] == 'mac-beta' and (not status()['space_open_pending']), 'switch beta')
    capture('beta')
    cli('space', 'open', 'mac-alpha', '--no-attach', '--no-run')
    wait(lambda: status()['space'] == 'mac-alpha' and (not status()['space_open_pending']), 'switch alpha')
    after = next((s for s in snapshot()['sessions'] if s['id'] == alpha['id']))['windows'][0]['panes'][0]
    assert (after['id'], after['child_pid']) == identity
    record('space-switch-keeps-session-and-child')
    receipt = json.loads(cli('restart', '--host'))
    assert any((c.get('response', {}).get('status') == 'restarted' for c in receipt['components'])), receipt
    wait(lambda: status()['space'] == 'mac-alpha', 'restore after host restart')
    time.sleep(1)
    capture('restart-1s')
    cli('pane-write', pane['id'], '--text', "printf 'RESTART_%s_OK\\n' 'REPAINT'", '--submit', 'enter', '--json')
    for seconds in [2, 5, 10]:
        time.sleep(seconds)
        capture('restart-after-' + str(seconds))
        (out / ('status-after-' + str(seconds) + '.json')).write_text(json.dumps(status(), indent=2))
    capture('host-restarted')
    after = next((s for s in snapshot()['sessions'] if s['id'] == alpha['id']))['windows'][0]['panes'][0]
    assert (after['id'], after['child_pid']) == identity
    record('host-restart-preserves-live-pty', receipt=receipt)
    receipt = json.loads(cli('restart', '--daemon'))
    assert receipt['components'][0]['status'] == 'deferred', receipt
    record('daemon-restart-defers-live-sessions')
    mcp = subprocess.Popen([str(bins / 'pmux-mcp'), '--as', 'mac-validation', '--supervise'], env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=(out / 'mcp.log').open('w'), text=True, bufsize=1)

    def rpc(data):
        mcp.stdin.write(json.dumps(data) + '\n')
        mcp.stdin.flush()
        if 'id' not in data:
            return
        import select
        assert select.select([mcp.stdout], [], [], 10)[0]
        reply = json.loads(mcp.stdout.readline())
        assert reply['id'] == data['id']
        return reply
    assert 'result' in rpc({'jsonrpc': '2.0', 'id': 1, 'method': 'initialize', 'params': {'protocolVersion': '2025-06-18', 'capabilities': {}, 'clientInfo': {'name': 'mac-validation', 'version': '1'}}})
    rpc({'jsonrpc': '2.0', 'method': 'notifications/initialized'})
    wait(lambda: json.loads(cli('restart', '--mcp', '--plan'))['mcp_supervisor_pids'], 'MCP supervisor')
    receipt = json.loads(cli('restart', '--mcp'))
    assert any((c.get('response', {}).get('status') == 'restarted' for c in receipt['components'])), receipt
    assert rpc({'jsonrpc': '2.0', 'id': 2, 'method': 'tools/list', 'params': {}})['result']['tools']
    record('mcp-restart-keeps-protocol', receipt=receipt)
    (out / 'result.json').write_text(json.dumps({'status': 'PASS', 'records': records}, indent=2))
except Exception as e:
    (out / 'result.json').write_text(json.dumps({'status': 'FAIL', 'error': repr(e), 'records': records}, indent=2))
    raise
finally:
    if mcp:
        mcp.stdin.close()
        try:
            mcp.wait(timeout=5)
        except subprocess.TimeoutExpired:
            mcp.terminate()
            mcp.wait(timeout=5)
    for p in [host, daemon]:
        if p and p.poll() is None:
            p.terminate()
            try:
                p.wait(timeout=5)
            except subprocess.TimeoutExpired:
                p.kill()
                p.wait(timeout=5)
    shutil.rmtree(runtime)
