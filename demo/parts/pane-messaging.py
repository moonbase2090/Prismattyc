#!/usr/bin/env python3
"""Demonstrate a real pane write and remove-and-kill after capture."""
import json, os, socket, subprocess, tempfile, time
from pathlib import Path


PMUX = os.environ.get('PRISMATTYC_DEMO_PMUX', 'pmux')
PMUX_SOCKET = os.environ['PMUX_SOCKET']


def run(*args):
    return subprocess.check_output(
        [PMUX, '--socket', PMUX_SOCKET, *args], text=True
    ).strip()


def demonstrate(view):
    name = 'pane-write-demo-' + str(os.getpid())
    session = name + '-shell'
    created = False
    try:
        run('space', 'create', name, '--session-name', session, '--no-attach', '--view-path', str(view))
        created = True
        with socket.socket(socket.AF_UNIX) as stream:
            stream.connect(PMUX_SOCKET)
            stream.sendall(b'{"version":1,"request_id":1,"type":"snapshot"}\n')
            snapshot = json.loads(stream.makefile().readline())['response']['snapshot']
        pane = next(s for s in snapshot['sessions'] if s['name'] == session)['windows'][0]['panes'][0]['id']
        print(f"Created a test shell in pane {pane}.", flush=True)
        print(f"\npmux pane-write {pane} --text 'seq 1 10000' --submit enter --json", flush=True)
        receipt = run('pane-write', str(pane), '--text', 'seq 1 10000', '--submit', 'enter', '--json')
        assert json.loads(receipt)['response']['complete'], receipt
        print(receipt, flush=True)
        for _ in range(50):
            lines = run('save-buffer', str(pane), '-').splitlines()
            if any(line.strip() == '10000' for line in lines):
                print('\nTarget pane output:', flush=True)
                print('\n'.join([line.rstrip() for line in lines if line.strip()][-8:]), flush=True)
                break
            time.sleep(.1)
        else:
            raise RuntimeError('target output did not reach 10000')
        cleanup_file = os.environ.get('PRISMATTYC_DEMO_CLEANUP')
        if cleanup_file:
            Path(cleanup_file).write_text(f'{name}\t{session}\n')
        print('The isolated test session will be removed after capture stops.', flush=True)
    except BaseException:
        if created:
            subprocess.run([PMUX, '--socket', PMUX_SOCKET, 'space', 'remove', name, '--session', session, '--kill'], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            subprocess.run([PMUX, '--socket', PMUX_SOCKET, 'space', 'rm', name], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        raise


if __name__ == '__main__':
    with tempfile.TemporaryDirectory(prefix='pane-write-reel-') as runtime:
        demonstrate(Path(runtime) / 'view.json')
    if os.environ.get('PRISMATTYC_DEMO_RESULT'):
        Path(os.environ['PRISMATTYC_DEMO_RESULT']).write_text('PASS\n')
