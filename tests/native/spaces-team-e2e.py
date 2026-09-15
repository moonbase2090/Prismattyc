#!/usr/bin/env python3
"""Black-box Spaces team checks. Uses private processes, sockets, and data."""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import shlex
import select
import socket
import subprocess
import sys
import tempfile
import time
sys.dont_write_bytecode = True


def wait(check, label, timeout=15):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        result = check()
        if result:
            return result
        time.sleep(.1)
    raise AssertionError('timeout: ' + label)


def main():
    out = Path(os.environ.get('SPACES_TEAM_OUT', 'build/spaces-team-e2e')).resolve()
    out.mkdir(parents=True, exist_ok=False)
    bins = {name: shutil.which(name) for name in ['pmux', 'pmuxd', 'prismattyc-host', 'pmux-mcp']}
    assert all(bins.values()), bins
    result = {'status': 'RUNNING', 'checks': [], 'sha256': {
        name: hashlib.sha256(Path(path).read_bytes()).hexdigest() for name, path in bins.items()}}
    daemon = display = host = None
    with tempfile.TemporaryDirectory(prefix='pmux-team-') as runtime:
        for key in list(os.environ):
            if key.startswith(('PMUX', 'PRISMATTYC_', 'XDG_')) or key == 'WAYLAND_DISPLAY':
                del os.environ[key]
        home = out / 'home'
        home.mkdir()
        os.environ.update(HOME=str(home), SHELL='/bin/bash', PMUX_SOCKET=runtime + '/pmux.sock',
                          PMUX_SERVER=bins['pmuxd'], XDG_RUNTIME_DIR=runtime, WINIT_UNIX_BACKEND='x11')
        for kind in ['data', 'config', 'state']:
            path = out / kind
            path.mkdir()
            os.environ['XDG_' + kind.upper() + '_HOME'] = str(path)
        spaces = out / 'data/prismattyc/spaces'

        def cli(*args, good=True):
            proc = subprocess.run([bins['pmux'], *map(str, args)], capture_output=True, text=True, timeout=30)
            with (out / 'commands.log').open('a') as log:
                log.write(json.dumps({'args': args, 'exit': proc.returncode, 'stdout': proc.stdout, 'stderr': proc.stderr}) + '\n')
            assert (proc.returncode == 0) == good, (args, proc.returncode, proc.stdout, proc.stderr)
            return proc.stdout if good else proc.stderr

        def snapshot():
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as stream:
                stream.settimeout(5)
                stream.connect(os.environ['PMUX_SOCKET'])
                stream.sendall(b'{"type":"snapshot","version":1,"request_id":1}\n')
                return json.loads(stream.makefile().readline())['response']['snapshot']

        def details(name):
            return json.loads(cli('space', 'details', name, '--json'))

        def identities():
            return {s['name']: (s['id'], s.get('space_id'), [(p['id'], p.get('child_pid')) for w in s['windows'] for p in w['panes']])
                    for s in snapshot()['sessions']}

        def passed(name):
            result['checks'].append({'name': name, 'status': 'PASS'})
            print('PASS ' + name, flush=True)

        try:
            daemon = subprocess.Popen([bins['pmuxd'], '--socket', os.environ['PMUX_SOCKET'], '--', '/bin/bash', '--noprofile', '--norc'],
                                      stdout=(out / 'daemon.log').open('w'), stderr=subprocess.STDOUT)
            wait(lambda: Path(os.environ['PMUX_SOCKET']).exists(), 'private daemon')
            cli('space', 'create', 'alpha', '--session-name', 'alpha-1', '--no-attach')
            cli('space', 'create', 'beta', '--session-name', 'beta-1', '--no-attach')
            before = identities()
            assert before['alpha-1'][0] != before['beta-1'][0]
            assert before['alpha-1'][2] != before['beta-1'][2]
            passed('independent-create')
            cli('space', 'role', 'alpha', 'alpha-1', 'Builder')
            cli('space', 'link', 'alpha', 'Repository', str(out))
            assert details('alpha')['sessions'][0]['role'] == 'Builder'
            assert details('alpha')['links']['Repository'] == str(out)
            cli('space', 'link', 'alpha', 'Unsafe', 'javascript:alert(1)', good=False)
            passed('roles-and-context-links')

            cli('mail', '--as', 'alpha-1', 'send', 'alpha-1', '--summary', 'Private fixture letter')
            inbox = cli('mail', '--as', 'alpha-1', 'inbox')
            cli('attention', 'alpha-1', 'Choose a branch')
            first = details('alpha')
            cli('attention', 'alpha-1', 'Choose a branch')
            report = details('alpha')
            assert report['sessions_needing_input'] == 1 and report['letters'] == 1, report
            assert report['sessions'][0]['attention'] == first['sessions'][0]['attention']
            request = report['sessions'][0]['attention'][0]
            assert cli('mail', '--as', 'alpha-1', 'inbox') == inbox
            passed('attention-dedup-and-separate-mail')
            cli('space', 'attention', 'snooze', 'alpha', 'alpha-1', request['pane_id'], request['revision'])
            assert details('alpha')['sessions_needing_input'] == 0
            assert cli('mail', '--as', 'alpha-1', 'inbox') == inbox
            passed('snooze-keeps-mail')
            cli('attention', 'alpha-1', 'Review the diff')
            cli('space', 'attention', 'resolve', 'alpha', 'alpha-1', request['pane_id'], request['revision'], good=False)
            assert details('alpha')['sessions_needing_input'] == 1
            passed('stale-attention-refused')
            request = details('alpha')['sessions'][0]['attention'][0]
            cli('space', 'move', 'beta', '--session', 'alpha-1')
            assert identities()['alpha-1'][0] == before['alpha-1'][0]
            assert identities()['alpha-1'][2] == before['alpha-1'][2]
            moved = next(s for s in details('beta')['sessions'] if s['name'] == 'alpha-1')
            assert moved['role'] == 'Builder' and moved['attention'][0]['revision'] == request['revision']
            cli('space', 'attention', 'resolve', 'alpha', 'alpha-1', request['pane_id'], request['revision'], good=False)
            cli('session', 'name', 'builder-new', '--session', 'alpha-1')
            renamed = next(s for s in details('beta')['sessions'] if s['name'] == 'builder-new')
            assert renamed['role'] == 'Builder'
            cli('space', 'attention', 'resolve', 'beta', 'builder-new', request['pane_id'], request['revision'])
            assert details('beta')['sessions_needing_input'] == 0
            assert cli('mail', '--as', 'builder-new', 'inbox') == inbox
            passed('move-rename-and-resolve-preserve-identity-and-mail')
            cli('space', 'link', 'beta', 'Repository', str(out))
            owner_before = details('beta')['space_id']
            cli('space', 'rename', 'beta', 'renamed-beta')
            assert details('renamed-beta')['space_id'] == owner_before
            assert details('renamed-beta')['links']['Repository'] == str(out)
            assert details('renamed-beta')['sessions'][1]['role'] == 'Builder'
            cli('space', 'rename', 'renamed-beta', 'beta')
            passed('space-rename-preserves-roles-and-context-links')

            counter = out / 'launch-count'
            saved_path = spaces / 'beta.json'
            saved = json.loads(saved_path.read_text())
            saved['sessions'][0]['windows'][0]['root']['command'] = "printf 'launched\\n' >> '" + str(counter) + "'"
            saved_path.write_text(json.dumps(saved))
            cli('space', 'template', 'save', 'builders', '--space', 'beta')
            pre_preview = identities()
            preview = json.loads(cli('space', 'template', 'preview', 'builders', 'gamma', '--json'))
            assert not preview['conflicts'] and not (spaces / 'gamma.json').exists()
            assert identities() == pre_preview and not counter.exists()
            passed('template-save-and-preview-have-no-launch-side-effects')
            conflict = json.loads(cli('space', 'template', 'preview', 'builders', 'beta', '--json'))
            assert conflict['conflicts']
            cli('space', 'template', 'create', 'builders', 'beta', '--prepare-only', good=False)
            assert identities() == pre_preview
            passed('template-conflicts-fail-before-mutation')
            cli('space', 'template', 'create', 'builders', 'gamma', '--prepare-only', '--launch')
            wait(lambda: counter.exists() and counter.read_text() == 'launched\n', 'one explicit launch')
            created = identities()
            cli('space', 'template', 'create', 'builders', 'gamma', '--prepare-only', '--launch')
            assert identities() == created and counter.read_text() == 'launched\n'
            assert created['gamma-1'][0] != created['beta-1'][0]
            passed('template-explicit-launch-and-idempotent-retry')
            cli('space', 'template', 'create', 'builders', 'delta', '--prepare-only')
            assert counter.read_text() == 'launched\n'
            assert details('delta')['sessions'][1]['role'] == 'Builder'
            passed('template-shell-only-creation-and-role-copy')
            if '--termwright' in sys.argv:
                termwright = shutil.which('termwright')
                assert termwright, 'Termwright is required'
                before_attach = identities()
                capture = subprocess.run([termwright, 'screenshot', '--cols', '100', '--rows', '24',
                    '--wait-for', 'space beta', '--timeout', '20', '--output', str(out / 'space-attach.png'),
                    '--', bins['pmux'], 'space', 'attach', 'beta', '--session', 'beta-1'],
                    capture_output=True, text=True, timeout=30)
                (out / 'termwright-attach.log').write_text(capture.stdout + capture.stderr)
                assert capture.returncode == 0, capture.stderr
                assert (out / 'space-attach.png').stat().st_size > 1000
                assert identities() == before_attach and counter.read_text() == 'launched\n'
                passed('termwright-space-attach-preserves-team-and-launch-count')

            precommit = json.loads(cli('space', 'template', 'preview', 'builders', 'recovered', '--json'))
            precommit['definition']['id'] = 'e' * 32
            journal = {'source': json.loads((spaces / 'team-templates/builders.json').read_text()),
                       'space': precommit['definition'], 'launch': False, 'launches': {}, 'complete': False}
            (spaces / 'template-runs/recovered.json').write_text(json.dumps(journal))
            cli('space', 'template', 'create', 'builders', 'recovered', '--prepare-only')
            assert details('recovered')['space_id'] == 'e' * 32
            assert details('recovered')['sessions'][1]['role'] == 'Builder'
            assert counter.read_text() == 'launched\n'
            passed('template-resumes-intent-before-definition-commit')
            run_path = spaces / 'template-runs/gamma.json'
            run = json.loads(run_path.read_text())
            run['complete'] = False
            run['launches'][next(iter(run['launches']))] = 'outcome unknown'
            run_path.write_text(json.dumps(run))
            error = cli('space', 'template', 'create', 'builders', 'gamma', '--prepare-only', '--launch', good=False)
            assert 'unknown' in error and counter.read_text() == 'launched\n'
            passed('interrupted-launch-fails-closed-without-replay')
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as stream:
                stream.settimeout(5)
                stream.connect(os.environ['PMUX_SOCKET'])
                reader = stream.makefile()
                def control(identifier, kind, **fields):
                    stream.sendall((json.dumps(dict(type=kind, version=1, request_id=identifier, **fields)) + '\n').encode())
                    reply = json.loads(reader.readline())
                    assert reply['status'] == 'ok', reply
                    return reply['response']
                control(1, 'register_client')
                gamma = next(s for s in snapshot()['sessions'] if s['name'] == 'gamma-1')
                window = gamma['windows'][0]
                control(2, 'split', window_id=window['id'], target_pane_id=window['panes'][0]['id'],
                        axis='horizontal', ratio=.5, spawn={'program': '/bin/bash', 'argv': ['--noprofile', '--norc'], 'env': {}, 'cwd': None}, client_id=None)
            changed = cli('space', 'template', 'create', 'builders', 'gamma', '--prepare-only', '--launch', good=False)
            assert 'layout changed' in changed and counter.read_text() == 'launched\n'
            passed('template-retry-refuses-changed-live-layout')

            mcp = subprocess.Popen([bins['pmux-mcp'], '--as', 'builder-new'], stdin=subprocess.PIPE,
                                   stdout=subprocess.PIPE, stderr=(out / 'mcp.log').open('w'), text=True, bufsize=1)
            def rpc(identifier, method, params):
                mcp.stdin.write(json.dumps({'jsonrpc': '2.0', 'id': identifier, 'method': method, 'params': params}) + '\n')
                mcp.stdin.flush()
                deadline = time.monotonic() + 30
                while time.monotonic() < deadline:
                    assert select.select([mcp.stdout], [], [], 30)[0], 'MCP response timed out'
                    line = mcp.stdout.readline()
                    assert line, 'MCP adapter exited'
                    response = json.loads(line)
                    if response.get('id') == identifier:
                        assert 'error' not in response, response
                        return response['result']
                raise AssertionError('no matching MCP response')
            try:
                rpc(1, 'initialize', {'protocolVersion': '2024-11-05', 'capabilities': {},
                                     'clientInfo': {'name': 'spaces-box', 'version': '1'}})
                mcp.stdin.write(json.dumps({'jsonrpc': '2.0', 'method': 'notifications/initialized'}) + '\n')
                mcp.stdin.flush()
                tools = rpc(2, 'tools/list', {})
                assert len(tools['tools']) == 18
                response = rpc(3, 'tools/call', {'name': 'pmux_space_details', 'arguments': {'name': 'beta'}})
                assert not response.get('isError'), response
                actual = json.loads(response['content'][0]['text'])
                expected = details('beta')
                actual.pop('observed_at_ms'); expected.pop('observed_at_ms')
                assert actual == expected
                response = rpc(4, 'tools/call', {'name': 'pmux_template_preview', 'arguments': {'template': 'builders', 'space': 'mcp-preview'}})
                assert not response.get('isError') and not (spaces / 'mcp-preview.json').exists()
                assert cli('mail', '--as', 'builder-new', 'inbox') == inbox
                passed('mcp-details-and-template-preview-match-cli')
            finally:
                mcp.stdin.close()
                mcp.wait(timeout=5)

            if '--native' in sys.argv:
                os.environ.update(HOST_UX_OUT=str(out / 'native'), HOST_UX_NO_WM='1')
                spec = importlib.util.spec_from_file_location('ux', Path(__file__).with_name('host-ux-e2e.py'))
                ux = importlib.util.module_from_spec(spec)
                spec.loader.exec_module(ux)
                display = subprocess.Popen(['Xvfb', '-displayfd', '1', '-screen', '0', '1920x1080x24', '-nolisten', 'tcp', '-noreset'],
                                           stdout=subprocess.PIPE, stderr=(out / 'xvfb.log').open('w'))
                os.environ['DISPLAY'] = ':' + display.stdout.readline().decode().strip()
                cli('space', 'open', 'beta', '--no-attach', '--no-run')
                os.environ['PMUX_SPACE'] = 'beta'
                host = ux.Host('team', ['--attach-session', str(identities()['beta-1'][0]), '--attach-session', str(identities()['builder-new'][0])], 'font_px = 16.0\nspace_rail = "bottom"\nwindow_opacity = 1.0\n[keys]\nspace_rail_focus = \"ctrl+alt+s\"\n')
                wait(lambda: host.status().get('space_session_names', {}).get('beta') == ['beta-1', 'builder-new'], 'session names in chip')
                host.capture('session-names')
                host.display('session-names')
                passed('native-session-names-in-chip')
                # Bind the existing rail action in the fixture, then use its menu.
                host.key('ctrl+alt+s')
                host.key('Menu')
                host.key('Home')
                for _ in range(7): host.key('Down')
                host.key('Return')
                wait(lambda: host.status().get('space_details') and not host.status()['space_details']['loading'], 'team details')
                host.capture('team-details')
                host.display('team-details')
                assert 'builder-new' in str(host.status()['space_details']), host.status()
                passed('native-keyboard-team-details')

                def panel_ready():
                    state = host.status().get('space_details')
                    return state if state and not state['loading'] else None

                def choose(prefix):
                    state = wait(lambda: (state if any(text.startswith(prefix + ':') for text in state['rows']) else None) if (state := panel_ready()) else None, 'panel row ' + prefix)
                    index = next(i for i, text in enumerate(state['rows']) if text.startswith(prefix + ':'))
                    host.key('Home')
                    for _ in range(index):
                        host.key('Down')
                    host.key('Return')
                    wait(lambda: (current := host.status().get('space_details')) is None or
                         (not current['loading'] and current != state), 'panel action ' + prefix)

                def type_value(value):
                    host.key('ctrl+a')
                    ux.run('xdotool', 'type', '--clearmodifiers', '--delay', '5', value)
                    before_input = wait(lambda: state if (state := panel_ready()) and value in str(state) else None, 'typed input')
                    host.key('Return')
                    wait(lambda: (current := panel_ready()) and current != before_input, 'submitted input')

                focused = host.status()['focused_session']
                choose('builder-new')
                choose('Set role')
                type_value('Reviewer')
                wait(lambda: details('beta')['sessions'][1]['role'] == 'Reviewer', 'role from keyboard')
                choose('Back to team')
                choose('Add context link')
                type_value('Design')
                type_value('javascript:invalid')
                wait(lambda: 'HTTP(S)' in str(panel_ready()) and 'javascript:invalid' in str(panel_ready()), 'invalid link remains editable')
                host.capture('editable-link-error')
                type_value(str(out))
                wait(lambda: details('beta')['links'].get('Design') == str(out), 'corrected context link')
                choose('Back to team')
                cli('attention', 'builder-new', 'Review the saved patch')
                wait(lambda: host.status()['space_attention_counts'].get('beta') == 1, 'attention chip count')
                host.key('Escape')
                wait(lambda: host.status().get('space_details') is None, 'Escape closes details')
                assert host.status()['focused_session'] == focused
                host.key('Escape')  # Return keyboard focus from the rail to the pane.
                host.key('ctrl+alt+s')
                host.key('Menu')
                host.key('Home')
                for _ in range(7): host.key('Down')
                host.key('Return')
                choose('builder-new')
                host.capture('attention-details')
                choose('Snooze for 10 minutes')
                wait(lambda: host.status()['space_attention_counts'].get('beta', 0) == 0, 'snooze clears reminder')
                assert cli('mail', '--as', 'builder-new', 'inbox') == inbox
                choose('Back to team')
                choose('builder-new')
                choose('View session')
                wait(lambda: host.status()['focused_session'] == str(identities()['builder-new'][0]), 'focus selected session')
                passed('native-role-link-retry-attention-and-focus')
                host.key('Escape')
                host.key('ctrl+alt+s')
                host.key('Menu')
                host.key('Home')
                for _ in range(7): host.key('Down')
                host.key('Return')
                choose('Team templates')
                choose('builders')
                type_value('epsilon')
                wait(lambda: 'Create shells only' in str(panel_ready()), 'native preview')
                assert not (spaces / 'epsilon.json').exists()
                host.capture('template-preview')
                choose('Create shells only')
                wait(lambda: host.status()['space'] == 'epsilon' and not host.status()['space_open_pending'], 'template opens intended view')
                assert counter.read_text() == 'launched\n'
                host.capture('template-created')
                retained = wait(lambda: json.loads(cli('space', 'result', 'epsilon', '--json')), 'retained result')
                assert retained[0]['name'] == 'epsilon' and retained[0]['view'] == 'applied', retained
                mcp = subprocess.Popen([bins['pmux-mcp'], '--as', 'builder-new'], stdin=subprocess.PIPE,
                                       stdout=subprocess.PIPE, stderr=(out / 'mcp-result.log').open('w'), text=True, bufsize=1)
                try:
                    rpc(10, 'initialize', {'protocolVersion': '2024-11-05', 'capabilities': {},
                                          'clientInfo': {'name': 'spaces-result-box', 'version': '1'}})
                    mcp.stdin.write(json.dumps({'jsonrpc': '2.0', 'method': 'notifications/initialized'}) + '\n')
                    mcp.stdin.flush()
                    response = rpc(11, 'tools/call', {'name': 'pmux_space_result', 'arguments': {'name': 'epsilon'}})
                    assert not response.get('isError') and json.loads(response['content'][0]['text']) == retained
                    passed('mcp-retained-open-result-matches-cli')
                finally:
                    mcp.stdin.close()
                    mcp.wait(timeout=5)
                host.key('Escape')
                host.key('ctrl+alt+s')
                host.key('Menu')
                host.key('Home')
                for _ in range(7): host.key('Down')
                host.key('Return')
                choose('Last open result')
                host.capture('last-result')
                passed('native-template-preview-create-and-retained-result')
                host.key('Escape')
                host.key('ctrl+alt+s')
                host.key('Home')
                host.key('Right')
                host.key('Menu')
                host.key('Home')
                for _ in range(7): host.key('Down')
                host.key('Return')
                choose('builder-new')
                choose('View session')
                wait(lambda: host.status()['space'] == 'beta' and host.status()['focused_session'] == str(identities()['builder-new'][0]), 'cross-Space session navigation')
                assert counter.read_text() == 'launched\n'
                preserved = identities()['builder-new']
                cli('stop', 'beta-1')
                host.key('ctrl+alt+s')
                host.key('Menu')
                host.key('Home')
                for _ in range(7): host.key('Down')
                host.key('Return')
                choose('beta-1')
                choose('Reopen session')
                wait(lambda: details('beta')['sessions'][0]['state'] == 'running', 'single session reopened')
                assert identities()['builder-new'] == preserved
                host.capture('single-session-reopened')
                passed('native-cross-space-focus-and-single-session-reopen')
                host.key('Escape')
                ux.run('xdotool', 'windowsize', '--sync', host.wid, '500', '600')
                time.sleep(1)
                host.key('Escape')
                host.key('ctrl+alt+s')
                host.key('Menu')
                host.key('Home')
                for _ in range(7): host.key('Down')
                host.key('Return')
                choose('Team templates')
                choose('builders')
                type_value('narrow-preview')
                wait(lambda: 'Command:' in str(panel_ready()), 'narrow launch recipe')
                host.capture('narrow-preview')
                host.display('narrow-preview')
                assert not (spaces / 'narrow-preview.json').exists()
                passed('native-narrow-preview')

                # Remove saved membership from the real right-click menu.
                host.key('Escape')
                ux.run('xdotool', 'windowsize', '--sync', host.wid, '1000', '600')
                cli('space', 'create', 'remove-menu', '--session-name', 'remove-me', '--no-attach')
                cli('space', 'open', 'remove-menu', '--no-attach', '--no-run')
                wait(lambda: host.status()['space'] == 'remove-menu' and not host.status()['space_open_pending'], 'remove menu space')
                cli('mail', '--as', 'remove-me', 'send', 'remove-me', '--summary', 'keep this letter', '--body', 'membership removal must preserve mail')
                pending_mail = cli('mail', '--as', 'remove-me', 'inbox')

                def remove_from_menu(label):
                    before = identities()['remove-me']
                    geometry = dict(line.split('=', 1) for line in ux.run('xdotool', 'getwindowgeometry', '--shell', host.wid).splitlines())
                    ux.run('xdotool', 'mousemove', '--sync', str(int(geometry['X']) + 40),
                           str(int(geometry['Y']) + 10), 'click', '3')
                    wait(lambda: host.status().get('context_menu') is not None, 'session tab context menu')
                    host.key('End')
                    host.key('Up')
                    time.sleep(.3)
                    host.capture(label + '-menu')
                    host.key('Return')
                    wait(lambda: not details('remove-menu')['sessions'], 'saved membership removed')
                    wait(lambda: identities()['remove-me'][1] is None, 'ownership released')
                    after = identities()['remove-me']
                    assert (before[0], before[2]) == (after[0], after[2]), (before, after)
                    assert cli('mail', '--as', 'remove-me', 'inbox') == pending_mail
                    wait(lambda: not host.status().get('focused_session'), 'removed session view detached')
                    time.sleep(.3)
                    host.capture(label + '-removed')
                    saved = json.loads((spaces / 'remove-menu.json').read_text())
                    assert not saved['sessions'] and not saved.get('tabs'), saved
                    passed('native-remove-' + label + '-from-session-context-menu')

                remove_from_menu('running')
                cli('space', 'add', 'remove-menu', '--session', 'remove-me')
                cli('space', 'open', 'remove-menu', '--no-attach', '--no-run')
                wait(lambda: host.status().get('focused_session') == str(identities()['remove-me'][0]), 'reattach before exit')
                remote = identities()['remove-me'][2][0][0]
                cli('send', remote, 'exit', '--enter', '--force')
                wait(lambda: not next(p for s in snapshot()['sessions'] if s['name'] == 'remove-me'
                                     for w in s['windows'] for p in w['panes']).get('child_pid'), 'exited session placeholder')
                time.sleep(.5)
                remove_from_menu('exited')

                # Reproduce closing tabs, saving the reduced Space, and reopening.
                cli('space', 'create', 'restart-clean', '--session-name', 'survivor', '--no-attach')
                for name in ['old-exited', 'old-running']:
                    cli('new', name, '--no-attach')
                    cli('space', 'add', 'restart-clean', '--session', name)
                cli('space', 'open', 'restart-clean', '--no-attach', '--no-run')
                wait(lambda: host.status()['space'] == 'restart-clean' and not host.status()['space_open_pending'], 'three session Space')
                before_restart = identities()
                cli('send', identities()['old-exited'][2][0][0], 'exit', '--enter', '--force')
                wait(lambda: identities()['old-exited'][2][0][1] is None, 'exited tab retained')
                for count, name in [(3, 'old-exited'), (2, 'old-running')]:
                    geometry = dict(line.split('=', 1) for line in ux.run('xdotool', 'getwindowgeometry', '--shell', host.wid).splitlines())
                    ux.run('xdotool', 'mousemove', '--sync', str(int(geometry['X']) + int(geometry['WIDTH']) * 3 // (count * 2)),
                           str(int(geometry['Y']) + 10), 'click', '1')
                    wait(lambda: host.status().get('focused_session') == str(identities()[name][0]), 'focus removed tab ' + name)
                    host.key('ctrl+shift+w')
                    wait(lambda: host.status().get('focused_session') != str(identities()[name][0]), 'close removed tab ' + name)
                host.key('ctrl+alt+s')
                host.key('Menu')
                host.key('Home')
                for _ in range(3): host.key('Down')
                host.key('Return')
                host.key('Return')
                wait(lambda: [s['name'] for s in details('restart-clean')['sessions']] == ['survivor'], 'save reduced Space')
                wait(lambda: identities()['old-exited'][1] is None and identities()['old-running'][1] is None, 'save releases omitted owners')
                assert identities()['old-running'][2] == before_restart['old-running'][2]
                host.capture('reduced-space-saved')
                host.stop()
                host = ux.Host('team-restarted', [], 'font_px = 16.0\nspace_rail = "bottom"\nwindow_opacity = 1.0\n')
                # The bare host shows its saved-view restore choice.
                time.sleep(.4)
                host.key('Return')
                wait(lambda: host.status()['space'] == 'restart-clean' and host.status().get('focused_session') == str(identities()['survivor'][0]), 'restore reduced Space')
                time.sleep(1.5)
                cache = json.loads(Path(os.environ['PMUX_SOCKET']).with_suffix('.attach-tabs.json').read_text())
                assert [key for tab in cache['tabs'] for key in tab['sessions']] == [str(identities()['survivor'][0])], cache
                assert host.status()['space_session_names']['restart-clean'] == ['survivor']
                assert identities()['survivor'][2] == before_restart['survivor'][2]
                host.capture('reopened-without-zombie-tabs')
                passed('native-remove-save-close-reopen-keeps-zombie-tabs-out')

                # Destructive removal requires confirmation and preserves mail.
                for ended in [False, True]:
                    name = 'kill-ended' if ended else 'kill-running'
                    cli('space', 'create', name, '--session-name', name, '--no-attach')
                    wait(lambda: host.status()['space'] == name and not host.status()['space_open_pending'], 'kill target view')
                    identity = identities()[name]
                    pid = identity[2][0][1]
                    cli('mail', '--as', name, 'send', name, '--summary', 'keep after kill', '--body', 'pending letter')
                    mail_before = cli('mail', '--as', name, 'inbox')
                    if ended:
                        cli('send', identity[2][0][0], 'exit', '--enter', '--force')
                        wait(lambda: identities()[name][2][0][1] is None, 'kill target exited')
                    def open_kill_menu():
                        geometry = dict(line.split('=', 1) for line in ux.run('xdotool', 'getwindowgeometry', '--shell', host.wid).splitlines())
                        ux.run('xdotool', 'mousemove', '--sync', str(int(geometry['X']) + 40),
                               str(int(geometry['Y']) + 10), 'click', '3')
                        wait(lambda: host.status().get('context_menu') is not None, 'kill context menu')
                        host.key('End')
                        host.key('Return')
                        wait(lambda: (host.status().get('context_menu') or {}).get('confirmation') == 11, 'kill confirmation armed')
                    open_kill_menu()
                    time.sleep(.3)
                    host.capture(name + '-confirm')
                    assert identities()[name][0] == identity[0]
                    assert [s['name'] for s in details(name)['sessions']] == [name]
                    host.key('Escape')
                    wait(lambda: host.status().get('context_menu') is None, 'kill confirmation cancelled')
                    assert name in identities(), 'cancel killed the session'
                    open_kill_menu()
                    host.key('Return')
                    wait(lambda: name not in identities(), 'session destroyed')
                    wait(lambda: not Path('/proc/' + str(pid)).exists(), 'session process terminated')
                    assert not details(name)['sessions']
                    assert cli('mail', '--as', name, 'inbox') == mail_before
                    wait(lambda: not host.status().get('focused_session'), 'killed session view removed')
                    host.capture(name + '-removed')
                    passed('native-confirmed-remove-and-' + name + '-preserves-mail')

                # Preferences, crowded rails, save state, and Undo use the real UI.
                cli('space', 'create', 'polish', '--session-name', 'polish-1', '--no-attach')
                wait(lambda: host.status()['space'] == 'polish', 'polish view')
                cfg = host.directory / 'config.toml'
                cfg.write_text(cfg.read_text() + '\n[keys]\nspace_settings = "ctrl+alt+p"\nundo_space_change = "ctrl+alt+u"\nsave_space = "ctrl+alt+v"\n')
                time.sleep(1)
                original = identities()['polish-1']
                host.key('ctrl+alt+p')
                wait(lambda: panel_ready() and panel_ready()['header'] == 'SPACES SETTINGS', 'settings action')
                for side in ['left', 'top', 'right', 'bottom']:
                    choose('Rail: ' + side)
                    wait(lambda: host.status()['space_rail_position'] == side, 'rail ' + side)
                    assert ('space_rail = "' + side + '"') in cfg.read_text()
                    host.key('Escape')
                    wait(lambda: host.status()['space_details'] is None, 'settings closed')
                    host.capture('rail-' + side)
                    chips = [c for c in host.status()['space_chips'] if c['name'] == 'polish']
                    assert chips, (side, host.status())
                    c = chips[0]
                    status = host.status()
                    if side == 'top':
                        assert c['y'] == 0, status
                        assert status['tab_strip_y'] == c['height'], status
                    else:
                        assert status['tab_strip_y'] == 0, status
                    g = dict(line.split('=', 1) for line in ux.run('xdotool', 'getwindowgeometry', '--shell', host.wid).splitlines())
                    ux.run('xdotool', 'mousemove', '--sync', str(int(g['X'])+c['x']+5), str(int(g['Y'])+c['y']+5), 'click', '3')
                    wait(lambda: host.status()['context_menu'] is not None, 'rail context click ' + side)
                    assert host.status()['context_menu']['target'] == 'space', host.status()
                    host.key('Escape')
                    wait(lambda: host.status()['context_menu'] is None, 'rail menu closed')
                    ux.run('xdotool', 'mousemove', '--sync', str(int(g['X']) + 40),
                           str(int(g['Y']) + status['tab_strip_y'] + 5), 'click', '3')
                    wait(lambda: host.status()['context_menu'] is not None, 'tab context click ' + side)
                    assert host.status()['context_menu']['target'] == 'pane', host.status()
                    host.key('Escape')
                    wait(lambda: host.status()['context_menu'] is None, 'tab menu closed')
                    # The single tab fills the right edge; no idle new-tab drop slot.
                    ux.run('xdotool', 'mousemove', '--sync', str(int(g['X']) + int(g['WIDTH']) - 6),
                           str(int(g['Y']) + status['tab_strip_y'] + 5), 'click', '3')
                    wait(lambda: host.status()['context_menu'] is not None, 'single tab right edge ' + side)
                    assert host.status()['context_menu']['target'] == 'pane', host.status()
                    host.key('Escape')
                    host.key('ctrl+alt+p')
                    wait(lambda: panel_ready(), 'settings reopen')
                assert identities()['polish-1'] == original
                passed('polish-four-rail-positions-live-config-and-hit-targets')
                choose('Startup: restore')
                assert 'space_startup = "restore"' in cfg.read_text()
                choose('Autosave: off')
                assert 'space_autosave = true' in cfg.read_text()
                host.capture('spaces-settings')
                host.key('Escape')
                cli('space', 'add', 'polish', '--name', 'polish-2')
                wait(lambda: host.status()['space_session_names']['polish'] == ['polish-1', 'polish-2'], 'second polish session')
                wait(lambda: host.status()['space_save_status'] == 'Saved', 'saved arrangement')
                host.key('ctrl+shift+w')
                wait(lambda: host.status()['space_save_status'] == 'Unsaved changes', 'unsaved arrangement')
                host.capture('unsaved-layout')
                wait(lambda: len(details('polish')['sessions']) == 1 and host.status()['space_save_status'] == 'Saved', 'autosave reduced view')
                host.capture('autosaved-layout')
                passed('polish-autosave-and-visible-save-state')
                host.key('ctrl+alt+p')
                wait(lambda: panel_ready(), 'autosave settings')
                choose('Autosave: on')
                host.key('Escape')
                cli('space','add','polish','--name','save-failure-seat')
                wait(lambda: len(host.status()['space_session_names']['polish']) == 2, 'save failure setup')
                host.key('ctrl+shift+w')
                wait(lambda: host.status()['space_save_status'] == 'Unsaved changes', 'manual save pending')
                before_failed_save = (spaces / 'polish.json').read_bytes()
                lock = spaces / '.ownership.lock'
                lock.chmod(0o400)
                try:
                    host.key('ctrl+alt+v')
                    wait(lambda: host.status()['space_save_status'] == 'Save failed', 'save failure visible')
                    assert (spaces / 'polish.json').read_bytes() == before_failed_save
                    host.capture('save-failed')
                finally: lock.chmod(0o600)
                host.key('ctrl+alt+v')
                wait(lambda: host.status()['space_save_status'] == 'Saved', 'manual save retry')
                passed('polish-save-failure-retains-definition-and-retry-recovers')
                # Remove only through the menu; Undo must restore the same live ID/PID.
                active = host.status()['focused_session']
                active_name = next(n for n, data in identities().items() if str(data[0]) == active)
                before_undo = identities()[active_name]
                g = dict(line.split('=',1) for line in ux.run('xdotool','getwindowgeometry','--shell',host.wid).splitlines())
                ux.run('xdotool','mousemove','--sync',str(int(g['X'])+35),str(int(g['Y'])+10),'click','3')
                wait(lambda: host.status()['context_menu'] is not None, 'grouped pane menu')
                host.capture('grouped-session-menu')
                host.key('Home')
                for _ in range(10): host.key('Down')
                host.key('Return')
                wait(lambda: host.status()['space_undo_available'] and not details('polish')['sessions'], 'remove with Undo')
                host.key('ctrl+alt+u')
                wait(lambda: bool(details('polish')['sessions']) and not host.status()['space_undo_available'], 'Undo removal')
                assert identities()[active_name] == before_undo
                host.capture('undo-restored')
                passed('polish-native-undo-restores-membership-without-respawn')
                # Crowded horizontal and vertical rails keep an overflow affordance.
                for i in range(16): cli('space','create',f'overflow-{i:02}', '--no-attach')
                cli('space','open','polish','--no-attach','--no-run')
                wait(lambda: host.status()['space'] == 'polish', 'return to polish')
                ux.run('xdotool','windowsize','--sync',host.wid,'500','500')
                time.sleep(.5)
                host.capture('rail-overflow-bottom')
                host.key('ctrl+alt+p')
                wait(lambda: panel_ready(), 'crowded settings')
                choose('Rail: left')
                host.key('Escape')
                wait(lambda: host.status()['space_rail_position'] == 'left', 'crowded left rail')
                host.capture('rail-overflow-left')
                # Overflow is the bottom rail cell on a side rail.
                g = dict(line.split('=',1) for line in ux.run('xdotool','getwindowgeometry','--shell',host.wid).splitlines())
                ux.run('xdotool','mousemove','--sync',str(int(g['X'])+8),str(int(g['Y'])+int(g['HEIGHT'])-8),'click','1')
                wait(lambda: host.status()['space_picker_open'], 'overflow picker opens')
                host.capture('overflow-picker')
                host.key('Escape')
                passed('polish-crowded-rail-overflow-picker')
                config_text = cfg.read_text()
                before_restore = identities()[active_name]
                host.stop()
                host = ux.Host('polish-restored', [], config_text)
                wait(lambda: host.status()['space'] == 'polish' and host.status()['focused_session'] == str(before_restore[0]), 'remembered restore without prompt')
                assert identities()[active_name] == before_restore
                host.capture('remembered-startup-restore')
                passed('polish-remembered-startup-reconnects-without-respawn')

                # Blank terminals are local shells. Automatic naming stays managed.
                before_blank = identities()
                before_tabs = host.status()['tab_count']
                for index, chord in enumerate(['ctrl+shift+t', 'ctrl+shift+e']):
                    host.key(chord)
                    wait(lambda: host.status()['session_prompt_open'], 'new terminal popup')
                    if index == 0:
                        host.capture('new-terminal-dialog')
                    host.key('Tab')
                    host.key('Return')
                    wait(lambda: not host.status()['session_prompt_open'] and len(host.status()['local_terminal_panes']) == index + 1, 'blank terminal created')
                    assert identities() == before_blank, 'blank terminal created a managed session'
                assert host.status()['tab_count'] == before_tabs + 1
                local_panes = host.status()['local_terminal_panes']
                ux.run('xdotool', 'type', '--clearmodifiers', 'echo BLANK_TERMINAL_READY')
                host.key('Return')
                time.sleep(.5)
                host.capture('blank-terminal')
                passed('native-blank-tab-and-pane-have-no-session-or-mailbox')
                host.key('ctrl+alt+p')
                wait(lambda: panel_ready(), 'naming settings')
                choose('Session names: automatic')
                assert 'session_naming = "auto"' in (host.directory / 'config.toml').read_text()
                host.capture('automatic-naming-settings')
                host.key('Escape')
                for index, chord in enumerate(['ctrl+shift+t', 'ctrl+shift+e']):
                    host.key(chord)
                    wait(lambda: len(identities()) == len(before_blank) + index + 1 and host.status()['focused_session'], 'automatic managed terminal')
                    assert not host.status()['session_prompt_open']
                time.sleep(1.5)
                assert set(host.status()['local_terminal_panes']) == set(local_panes), 'Space refresh discarded blank terminals'
                host.capture('automatically-named-terminals')
                passed('native-auto-naming-skips-popup-and-preserves-local-terminals')
                config_text = (host.directory / 'config.toml').read_text()
                host.stop()
                host = ux.Host('naming-restored', [], config_text)
                wait(lambda: host.status()['space'] == 'polish' and host.status()['focused_session'], 'naming preference restart')
                before_auto = len(identities())
                host.key('ctrl+shift+t')
                wait(lambda: len(identities()) == before_auto + 1 and host.status()['focused_session'], 'automatic naming after restart')
                assert not host.status()['session_prompt_open']
                host.key('ctrl+alt+p')
                wait(lambda: panel_ready(), 'ask preference')
                choose('Session names: ask')
                host.key('Escape')
                host.key('ctrl+shift+t')
                wait(lambda: host.status()['session_prompt_open'], 'asking restored')
                host.key('Escape')
                assert len(identities()) == before_auto + 1
                passed('native-naming-preference-persists-and-ask-can-be-restored')

                def blank(chord):
                    count = len(host.status()['local_terminal_panes'])
                    host.key(chord)
                    wait(lambda: host.status()['session_prompt_open'], 'blank Space-view popup')
                    host.key('Tab')
                    host.key('Return')
                    wait(lambda: not host.status()['session_prompt_open'] and len(host.status()['local_terminal_panes']) == count + 1, 'blank Space-view terminal')

                def view_state():
                    state = host.status()
                    locals_ = set(state['local_terminal_panes'])
                    return {key: state[key] for key in ['tab_layouts', 'focused_pane', 'visible_pane_slots']} | {
                        'local_pids': sorted((p['pane_id'], p['local_child_pid']) for p in state['current_panes'] if p['pane_id'] in {f'PaneId({pane})' for pane in locals_})}

                def open_space(name):
                    cli('space', 'open', name, '--no-attach', '--no-run')
                    wait(lambda: host.status()['space'] == name and not host.status()['space_open_pending'], 'open local view ' + name)
                    time.sleep(1.2)

                cli('space', 'create', 'scratch-a', '--session-name', 'scratch-a-1', '--no-attach')
                wait(lambda: host.status()['space'] == 'scratch-a', 'scratch A created')
                blank('ctrl+shift+e')
                blank('ctrl+shift+e')
                blank('ctrl+shift+t')
                host.key('ctrl+shift+1')
                time.sleep(1.2)
                # This output arrives only after the view is hidden.
                parked_go = home / 'parked-go'
                parked_done = home / 'parked-done'
                ux.run('xdotool', 'type', '--clearmodifiers', f'(while [ ! -f {shlex.quote(str(parked_go))} ]; do sleep .1; done; echo HIDDEN_OUTPUT_A; echo done > {shlex.quote(str(parked_done))}) &')
                host.key('Return')
                time.sleep(.3)
                state_a = view_state()
                assert len(state_a['local_pids']) == 3
                assert host.status()['tab_count'] == 2
                host.capture('space-a-before-switch')
                cli('space', 'create', 'scratch-b', '--session-name', 'scratch-b-1', '--no-attach')
                wait(lambda: host.status()['space'] == 'scratch-b', 'scratch B created')
                time.sleep(1.2)
                assert not host.status()['local_terminal_panes'], host.status()
                assert host.status()['pane_count'] == 1 and host.status()['tab_count'] == 1, host.status()
                host.capture('space-b-without-a-terminals')
                parked_go.touch()
                wait(lambda: parked_done.exists(), 'hidden shell continues running')
                blank('ctrl+shift+t')
                time.sleep(1.2)
                state_b = view_state()
                assert len(state_b['local_pids']) == 1
                for _ in range(2):
                    open_space('scratch-a')
                    assert view_state() == state_a, (view_state(), state_a)
                    host.capture('space-a-restored')
                    open_space('scratch-b')
                    assert view_state() == state_b, (view_state(), state_b)
                host.capture('space-b-restored')
                passed('native-blank-terminals-stay-in-owner-view-with-layout-focus-and-pids')
                cli('space', 'rename', 'scratch-a', 'renamed-scratch')
                cli('space', 'create', 'scratch-a', '--session-name', 'replacement-a', '--no-attach')
                wait(lambda: host.status()['space'] == 'scratch-a', 'replacement A created')
                assert not host.status()['local_terminal_panes'], 'a reused Space name inherited old blank terminals'
                open_space('renamed-scratch')
                # A longer name can widen the vertical rail. The split tree,
                # focus, and processes remain fixed within the new viewport.
                restored = view_state()
                assert {k: v for k, v in restored.items() if k != 'visible_pane_slots'} == {k: v for k, v in state_a.items() if k != 'visible_pane_slots'}, (restored, state_a)
                host.capture('renamed-space-retains-blank-terminals')
                passed('native-blank-view-follows-stable-owner-through-rename-and-name-reuse')

                # Creation order survives an old Space being saved and renamed.
                order = host.status()['space_order']
                assert order.index('renamed-scratch') < order.index('scratch-b') < order.index('scratch-a'), order
                cli('space', 'save', 'renamed-scratch')
                time.sleep(1.5)
                assert host.status()['space_order'] == order
                host.capture('oldest-first-space-order')
                passed('native-space-order-is-oldest-first-after-save-rename-and-name-reuse')

                repo = out / 'git-label-repo'
                repo.mkdir()
                def git(*args):
                    subprocess.run(['git', '-C', str(repo), *args], check=True, capture_output=True)
                git('init', '-b', 'main')
                (repo / 'tracked').write_text('initial')
                git('add', 'tracked')
                git('-c', 'user.name=Test', '-c', 'user.email=test@example.invalid', 'commit', '-m', 'initial')
                blank('ctrl+shift+t')
                def shell(command):
                    ux.run('xdotool', 'type', '--clearmodifiers', command)
                    host.key('Return')
                shell('cd ' + str(repo))
                wait(lambda: host.status()['tab_git_labels'][-1] == 'git-label-repo:main', 'clean Git branch')
                host.capture('git-clean-branch')
                (repo / 'untracked').write_text('dirty')
                wait(lambda: host.status()['tab_git_labels'][-1] == 'git-label-repo:main *', 'dirty Git marker')
                host.capture('git-dirty-branch')
                git('checkout', '-b', 'feature/spaces')
                wait(lambda: host.status()['tab_git_labels'][-1] == 'git-label-repo:feature/spaces *', 'Git branch switch')
                blank('ctrl+shift+e')
                shell('cd ' + str(out))
                wait(lambda: host.status()['tab_git_labels'][-1] is None, 'non-repository pane clears Git label')
                host.capture('git-non-repository-pane')
                # Select the first pane in this tab by its rendered terminal slot.
                pane, slot = host.status()['visible_pane_slots'][0]
                ux.run('xdotool', 'mousemove', '--window', str(host.wid), str(slot[0] + 20), str(slot[1] + 40))
                ux.run('xdotool', 'click', '1')
                wait(lambda: host.status()['tab_git_labels'][-1] == 'git-label-repo:feature/spaces *', 'Git label follows focused pane')
                host.capture('git-follows-focused-pane')
                passed('native-git-label-tracks-cwd-branch-dirty-state-and-pane-focus')
                open_space('scratch-a')
                shell('cd ' + str(repo))
                wait(lambda: host.status()['tab_git_labels'][0] == 'git-label-repo:feature/spaces *', 'managed session Git label')
                host.capture('git-managed-session')
                passed('native-managed-session-git-label-uses-live-child-directory')





            cli('attention', 'builder-new', 'Retain this reason while disconnected')
            details('beta')
            daemon.terminate()
            daemon.wait(timeout=5)
            daemon = None
            disconnected = details('beta')
            saved_request = next(s for s in disconnected['sessions'] if s['name'] == 'builder-new')
            assert saved_request['state'].startswith('unknown') and 'stale' in disconnected['source']
            assert saved_request['attention'][0]['message'] == 'Retain this reason while disconnected'
            passed('disconnected-attention-retains-labeled-stale-reason')
            offline_preview = json.loads(cli('space', 'template', 'preview', 'builders', 'offline-team', '--json'))
            assert not offline_preview['conflicts'] and offline_preview['notices']
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as probe:
                assert probe.connect_ex(os.environ['PMUX_SOCKET']) != 0, 'preview started a daemon'
            assert not (spaces / 'offline-team.json').exists()
            passed('offline-template-preview-does-not-start-daemon-or-sessions')
            print(cli('space', 'details', 'beta'), flush=True)
            result['status'] = 'PASS'
            print('SPACES_TEAM_E2E_COMPLETE', flush=True)
        except Exception as error:
            import traceback
            result.update(status='FAIL', error=str(error), traceback=traceback.format_exc())
            raise
        finally:
            if host is not None:
                host.stop()
            if display is not None:
                display.terminate()
                display.wait(timeout=5)
            if daemon is not None:
                daemon.terminate()
                daemon.wait(timeout=5)
            (out / 'result.json').write_text(json.dumps(result, indent=2) + '\n')


if __name__ == '__main__':
    main()
