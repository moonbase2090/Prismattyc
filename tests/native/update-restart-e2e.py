#!/usr/bin/env python3
"""Check Update and Restart end to end on a private daemon.

The run uses the same steps as the host's update flow: save a session
configuration, restart the daemon with consent (stopping every session),
then restore. It proves that sessions come back running their programs
rather than as bare shells, and that a Space reopens after a plain daemon
restart too. It starts no window and never touches the real socket.
"""
import argparse
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.dont_write_bytecode = True

PROGRAMS = {'alpha': 'sleep 31415', 'beta': 'sleep 27182'}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bins', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    args = parser.parse_args()
    bins = args.bins.resolve()
    args.out.mkdir(parents=True, exist_ok=False)
    records = []
    tmp = tempfile.mkdtemp(prefix='pmux-upd-', dir='/tmp')
    root = Path(tmp)
    try:
        sock = root / 'pmux.sock'
        project = root / 'project'
        project.mkdir()
        env = {k: v for k, v in os.environ.items() if not k.startswith(('PMUX', 'PRISMATTYC_', 'XDG_'))}
        env.update(HOME=tmp, SHELL='/bin/sh', XDG_DATA_HOME=str(root / 'data'),
                   XDG_CONFIG_HOME=str(root / 'config'), XDG_STATE_HOME=str(root / 'state'),
                   XDG_RUNTIME_DIR=tmp, PMUX_SOCKET=str(sock), PMUX_SERVER=str(bins / 'pmuxd'),
                   PATH=str(bins) + os.pathsep + env['PATH'])

        def cli(*argv, success=True, cwd=None):
            result = subprocess.run([str(bins / 'pmux'), *argv], env=env, cwd=cwd, capture_output=True,
                                    text=True, timeout=60)
            records.append(dict(args=argv, status=result.returncode, stdout=result.stdout, stderr=result.stderr))
            assert (result.returncode == 0) == success, records[-1]
            return result.stdout

        def snapshot():
            with socket.socket(socket.AF_UNIX) as conn:
                conn.settimeout(5)
                conn.connect(str(sock))
                conn.sendall(b'{"type":"snapshot","version":1,"request_id":1}\n')
                reply = json.loads(conn.makefile().readline())
                assert reply['status'] == 'ok', reply
                return {row['name']: row for row in reply['response']['snapshot']['sessions']}

        def processes():
            rows = {}
            for line in subprocess.run(['ps', '-A', '-o', 'pid=,ppid=,args='], capture_output=True, text=True).stdout.splitlines():
                pid, ppid, command = line.split(None, 2)
                rows[int(pid)] = (int(ppid), command)
            return rows

        def running(name):
            """Command lines of the pane's child and everything under it."""
            row = snapshot().get(name)
            if row is None:
                return None
            root_pid = row['windows'][0]['panes'][0]['child_pid']
            table = processes()
            found, queue = [], [root_pid]
            while queue:
                pid = queue.pop()
                if pid in table:
                    found.append(table[pid][1])
                queue.extend(child for child, (parent, _) in table.items() if parent == pid)
            return found

        def wait(check, label, timeout=30):
            end = time.monotonic() + timeout
            last = None
            while time.monotonic() < end:
                try:
                    last = check()
                    if last:
                        return last
                except (OSError, KeyError, IndexError, AssertionError):
                    pass
                time.sleep(0.2)
            raise AssertionError('timeout: ' + label + ' ' + repr(last))

        def runs(name):
            return wait(lambda: PROGRAMS[name] in ' '.join(running(name) or []), name + ' runs ' + PROGRAMS[name])

        def launches(name):
            path = project / (name + '.launches')
            return path.read_text().count('run\n') if path.exists() else 0

        def build_space():
            cli('up')
            cli('space', 'create', 'desk', '--session-name', 'shell', '--no-attach', cwd=project)
            for name, program in PROGRAMS.items():
                # The launch line counts how many times this pane's program started.
                script = f"echo run >> {name}.launches; exec {program}"
                cli('new', '--no-attach', name, '/bin/sh', '-c', script, cwd=project)
                cli('space', 'add', 'desk', '--session', name)
                runs(name)
            listing = cli('space', 'ls')
            assert 'desk  3 sessions' in listing, listing

        def settle_and_save(path):
            deadline = time.monotonic() + 30
            while True:
                path.unlink(missing_ok=True)
                cli('session-config', 'save', str(path))
                time.sleep(0.5)
                try:
                    cli('session-config', 'check', str(path))
                    return
                except AssertionError:
                    assert time.monotonic() < deadline, 'sessions never settled'

        def assert_back(label, expected_launches):
            for name, program in PROGRAMS.items():
                assert program in ' '.join(wait(lambda: running(name), name + ' present after ' + label)), (label, name)
            shell_only = ' '.join(running('shell') or [])
            assert not any(program in shell_only for program in PROGRAMS.values()), (label, shell_only)
            # The wrapper's launch line is written by the first restore only; a
            # later restore replays the program alone. Neither may replay more
            # than once, and each pane runs exactly one copy of its program.
            time.sleep(1)  # A replay would show up as a further launch line.
            for name, program in PROGRAMS.items():
                assert 2 <= launches(name) <= expected_launches, (label, name, launches(name))
                copies = [c for c in running(name) if c.endswith(program)]
                assert len(copies) == 1, (label, name, copies)
            listing = cli('space', 'ls')
            assert 'desk  3 sessions' in listing, (label, listing)
            checks.append(label)

        checks = []
        status = 'FAIL'
        try:
            build_space()
            before = json.loads(cli('versions'))['daemon']['pid']
            for name in PROGRAMS:
                assert launches(name) == 1, (name, launches(name))

            # Update and Restart: save, restart with consent, restore.
            consent = root / 'consent.json'
            settle_and_save(consent)
            reply = json.loads(cli('restart', '--daemon', '--stop-sessions', '--consented-config', str(consent),
                                   '--worker', '--json'))
            assert reply['status'] == 'restarted' or reply['components'][0]['status'] == 'restarted', reply
            after = json.loads(cli('versions'))['daemon']['pid']
            assert after != before, 'the daemon was not replaced'
            cli('session-config', 'restore', str(consent))
            assert_back('update_restart_restores_programs_not_bare_shells', 2)
            time.sleep(2)  # Autosave ticks on the new daemon must keep every member.
            assert 'desk  3 sessions' in cli('space', 'ls')
            checks.append('space_keeps_members_after_restart')

            # A second restart of the restored daemon is as good as the first.
            settle_and_save(root / 'consent-2.json')
            cli('restart', '--daemon', '--stop-sessions', '--consented-config', str(root / 'consent-2.json'),
                '--worker', '--json')
            cli('session-config', 'restore', str(root / 'consent-2.json'))
            assert_back('second_update_restart_restores_programs', 3)

            # Space reopen after a plain daemon restart: stop everything, start
            # a new daemon and open the saved Space.
            cli('stop')
            assert not sock.exists(), 'the daemon is still running'
            cli('up')
            assert snapshot() == {} or 'alpha' not in snapshot(), 'sessions survived a stop'
            cli('space', 'open', 'desk', '--no-attach')
            assert_back('space_reopen_after_daemon_restart_runs_programs', 4)
            status = 'PASS'
            print('UPDATE_RESTART_E2E_COMPLETE: ' + ', '.join(checks), flush=True)
        finally:
            subprocess.run([str(bins / 'pmux'), 'stop'], env=env, stdout=subprocess.DEVNULL,
                           stderr=subprocess.DEVNULL, timeout=30)
            (args.out / 'result.json').write_text(json.dumps(dict(status=status, checks=checks, records=records), indent=2) + '\n')
    finally:
        # Nothing may outlive the run: no process using the private root and
        # no file left in it, whether the run passed or failed.
        deadline = time.monotonic() + 15
        while True:
            leftover = [line for line in subprocess.run(['ps', '-A', '-o', 'pid=,args='], capture_output=True,
                                                         text=True).stdout.splitlines() if tmp in line]
            if not leftover or time.monotonic() > deadline:
                break
            time.sleep(0.2)
        shutil.rmtree(tmp, ignore_errors=True)
        assert not leftover, 'processes outlived the test: ' + repr(leftover)
        assert not os.path.exists(tmp), 'files left in ' + tmp


if __name__ == '__main__':
    main()
