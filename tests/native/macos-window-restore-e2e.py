#!/usr/bin/env python3
"""Prove native Dock restoration with two isolated macOS windows."""
# SPDX-License-Identifier: MIT
import ctypes as C
import json, os, pathlib, shutil, subprocess, tempfile, time
import argparse
import sys
assert sys.platform == "darwin", "Run on a logged-in macOS desktop with Accessibility permission"
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--bins', type=pathlib.Path, required=True)
parser.add_argument('--out', type=pathlib.Path, required=True)
parser.add_argument('--revision', required=True, help='expected full source commit')
args = parser.parse_args()
bins = args.bins.resolve()
out = args.out.resolve()
out.mkdir(parents=True, exist_ok=False)
runtime = pathlib.Path(tempfile.mkdtemp(prefix='minw-', dir='/tmp'))
macos = out / 'fixture-bin'
macos.mkdir(parents=True)
shutil.copy2(bins / 'prismattyc-host', macos / 'WindowRestoreFixture')
for name in ('pmux', 'pmuxd'):
    (macos / name).symlink_to(bins / name)
for name in ('home', 'config', 'data', 'state'):
    (out / name).mkdir()
env = {k: v for k, v in os.environ.items() if not k.startswith(('PMUX', 'PRISMATTYC_', 'XDG_'))}
env.pop('BASH_ENV', None)
env.pop('ENV', None)
env.update(PS1='fixture$ ', PROMPT_COMMAND='', HOME=str(out / 'home'), SHELL='/bin/bash', PATH=str(bins)+':/usr/bin:/bin:/usr/sbin:/sbin', PMUX_SOCKET=str(runtime / 'pmux.sock'), XDG_RUNTIME_DIR=str(runtime), XDG_CONFIG_HOME=str(out / 'config'), XDG_DATA_HOME=str(out / 'data'), XDG_STATE_HOME=str(out / 'state'))
config = out / 'fixture.toml'
config.write_text('automatic_update_checks = false\nspace_startup = "fresh"\n')
env['PRISMATTYC_CONFIG'] = str(config)
def cli(*args):
    p = subprocess.run([str(bins / 'pmux'), *args], env=env, capture_output=True, text=True, timeout=15)
    with (out / 'commands.jsonl').open('a') as f:
        f.write(json.dumps(dict(args=args, exit=p.returncode, stdout=p.stdout, stderr=p.stderr))+'\n')
    p.check_returncode()
    return p.stdout
host = daemon = None
print(out, flush=True)
try:
    daemonlog = (out / 'daemon.log').open('w')
    daemon = subprocess.Popen([str(bins / 'pmuxd'), '--socket', env['PMUX_SOCKET'], '--', '/bin/bash', '--noprofile', '--norc'], env=env, stdout=daemonlog, stderr=daemonlog)
    end = time.monotonic()+20
    while not pathlib.Path(env['PMUX_SOCKET']).exists():
        assert time.monotonic()<end, 'daemon socket timeout'
        time.sleep(.1)
    hostlog = (out / 'host.log').open('w')
    host = subprocess.Popen([str(macos / 'WindowRestoreFixture'), '--no-splash', '--', '/bin/bash', '--noprofile', '--norc'], env=env, stdout=hostlog, stderr=hostlog)
    end = time.monotonic()+40
    while True:
        assert host.poll() is None, 'host exited before render'
        assert time.monotonic()<end, 'render status timeout'
        try:
            status = json.loads(cli('render-status', '--json'))
            assert status.get('host_git_hash') == args.revision[:12], (
                'wrong host build', status.get('host_git_hash'), args.revision[:12])
            if (status['host_pid'] == host.pid and status.get('windows')
                    and status['windows'][0].get('last_raster', {}).get('present_succeeded')):
                break
        except (subprocess.CalledProcessError, ValueError, KeyError):
            pass
        time.sleep(.2)
    (out / 'before-status.json').write_text(json.dumps(status, indent=2))
    quartz = C.CDLL('/System/Library/Frameworks/CoreGraphics.framework/CoreGraphics')
    quartz.CGEventCreateKeyboardEvent.argtypes = [C.c_void_p, C.c_uint16, C.c_bool]
    quartz.CGEventCreateKeyboardEvent.restype = C.c_void_p
    quartz.CGEventSetFlags.argtypes = [C.c_void_p, C.c_uint64]
    quartz.CGEventPostToPid.argtypes = [C.c_int, C.c_void_p]
    quartz.CFRelease.argtypes = [C.c_void_p]
    for pressed in (True, False):
        event = quartz.CGEventCreateKeyboardEvent(None, 45, pressed)
        quartz.CGEventSetFlags(event, 1 << 20)
        quartz.CGEventPostToPid(host.pid, event)
        quartz.CFRelease(event)
    time.sleep(2)
    script = f'''on captureFixture(fixture, targetPath)
tell application "System Events" to tell fixture
set p to position of front window
set s to size of front window
end tell
set region to (item 1 of p as text) & "," & (item 2 of p as text) & "," & (item 1 of s as text) & "," & (item 2 of s as text)
do shell script "/usr/sbin/screencapture -x -R " & region & " " & quoted form of targetPath
end captureFixture

tell application "System Events"
set fixture to first application process whose unix id is {host.pid}
tell fixture
set frontmost to true
delay 1
repeat 60 times
if (count of windows) is 2 then exit repeat
delay 0.1
end repeat
if (count of windows) is not 2 then error "Expected two fixture windows, saw " & (count of windows)
log "stage: two visible windows before screenshot"
my captureFixture(fixture, {json.dumps(str(out / "before-minimize.png"))})
log "stage: initial window lookup after screenshot"
set targetWindow to front window
log "stage: initial minimize button"
click (first button of targetWindow whose subrole is "AXMinimizeButton")
delay 2
set beforeRestore to value of attribute "AXMinimized" of every window
if beforeRestore does not contain true then error "No minimized fixture window"
if beforeRestore does not contain false then error "Expected a visible sibling window"
log "Before Dock click"
log beforeRestore
my captureFixture(fixture, {json.dumps(str(out / "minimized-sibling.png"))})
end tell
tell process "Dock"
perform action "AXPress" of UI element "WindowRestoreFixture" of list 1
end tell
delay 2
tell fixture
set afterRestore to value of attribute "AXMinimized" of every window
if afterRestore contains true then error "Dock click left a fixture window minimized"
if (count of windows) is not 2 then error "Dock click changed window count"
log "After Dock click"
log afterRestore
my captureFixture(fixture, {json.dumps(str(out / "after-restore.png"))})
end tell
tell process "Dock" to perform action "AXPress" of UI element "WindowRestoreFixture" of list 1
delay 1
tell fixture
if (value of attribute "AXMinimized" of every window) contains true then error "Dock minimized a visible window"
if (count of windows) is not 2 then error "Dock created a window during normal activation"
repeat 2 times
repeat with w in windows
if (value of attribute "AXMinimized" of w) is false then
click (first button of w whose subrole is "AXMinimizeButton")
exit repeat
end if
end repeat
delay 1
end repeat
if (value of attribute "AXMinimized" of every window) contains false then error "Expected all fixture windows minimized"
end tell
tell process "Dock" to perform action "AXPress" of UI element "WindowRestoreFixture" of list 1
delay 1
tell fixture
if (value of attribute "AXMinimized" of every window) does not contain false then error "Dock left all windows minimized"
if (value of attribute "AXMinimized" of every window) does not contain true then error "Dock restored more than one minimized window"
if (count of windows) is not 2 then error "All-minimized restore changed window count"
click (first button of front window whose subrole is "AXCloseButton")
delay 1
if (count of windows) is not 1 then error "Expected one remaining window"
log "stage: last minimized window remains"
if (value of attribute "AXMinimized" of window 1) is not true then error "Expected the last window to remain minimized"
end tell
tell process "Dock" to perform action "AXPress" of UI element "WindowRestoreFixture" of list 1
delay 1
tell fixture
if (value of attribute "AXMinimized" of every window) contains true then error "Dock left the last window minimized"
if (count of windows) is not 1 then error "Last-window restore changed window count"
return "PASS visible sibling, normal activation, all minimized, and last-window restoration"
end tell
end tell'''
    (out / 'drive.applescript').write_text(script)
    result = subprocess.run(['osascript', str(out / 'drive.applescript')], capture_output=True, text=True, timeout=50)
    (out / 'native-result.json').write_text(json.dumps(dict(exit=result.returncode, stdout=result.stdout, stderr=result.stderr), indent=2))
    print(result.stdout, result.stderr, flush=True)
    (out / 'after-status.json').write_text(cli('render-status', '--json'))
    result.check_returncode()
    (out / 'result.json').write_text(json.dumps({'status': 'PASS', 'check': 'Dock restores a minimized window with a visible sibling', 'checks': ['visible-sibling', 'normal-activation', 'all-minimized', 'last-window']}))
finally:
    for process in (host, daemon):
        if process and process.poll() is None:
            process.terminate()
            try: process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill(); process.wait()
    shutil.rmtree(runtime)
    (out / 'cleanup.json').write_text(json.dumps({'host_reaped': host is None or host.poll() is not None, 'daemon_reaped': daemon is None or daemon.poll() is not None, 'runtime_removed': not runtime.exists()}))
