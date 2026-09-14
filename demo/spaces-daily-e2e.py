#!/usr/bin/env python3
"""Native acceptance checks for Space rail sizing and local terminal workflows."""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import hashlib
import shutil
sys.dont_write_bytecode = True

def wait(check, label, timeout=20):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        result = check()
        if result: return result
        time.sleep(.1)
    raise AssertionError('timeout: ' + label)

def main():
    out = Path(os.environ['DAILY_SPACES_OUT']).resolve()
    out.mkdir(parents=True, exist_ok=False)
    result = {'status': 'RUNNING', 'checks': [], 'sha256': {name: hashlib.sha256(Path(shutil.which(name)).read_bytes()).hexdigest() for name in ['pmux','pmuxd','prismattyc-host']}}
    host = daemon = display = None
    with tempfile.TemporaryDirectory(prefix='pmux-daily-') as runtime:
        for key in list(os.environ):
            if key.startswith(('PMUX','PRISMATTYC_','XDG_')) or key == 'WAYLAND_DISPLAY': del os.environ[key]
        home = out/'home'; home.mkdir()
        os.environ.update(HOME=str(home),SHELL='/bin/bash',PMUX_SOCKET=runtime+'/pmux.sock',XDG_RUNTIME_DIR=runtime,WINIT_UNIX_BACKEND='x11',HOST_UX_NO_WM='1',HOST_UX_OUT=str(out/'native'))
        for kind in ['data','config','state']:
            path=out/kind;path.mkdir();os.environ['XDG_'+kind.upper()+'_HOME']=str(path)
        def cli(*args):
            p=subprocess.run(['pmux',*map(str,args)],capture_output=True,text=True,timeout=30)
            with (out/'commands.log').open('a') as f:f.write(json.dumps({'args':args,'exit':p.returncode,'stdout':p.stdout,'stderr':p.stderr})+'\n')
            assert p.returncode == 0,(args,p.stderr)
            return p.stdout
        def sessions():
            import socket
            with socket.socket(socket.AF_UNIX,socket.SOCK_STREAM) as stream:
                stream.connect(os.environ['PMUX_SOCKET']);stream.sendall(b'{"type":"snapshot","version":1,"request_id":1}\n')
                data=json.loads(stream.makefile().readline())['response']['snapshot']['sessions']
                return {s['name']:(s['id'],[p.get('child_pid') for w in s['windows'] for p in w['panes']]) for s in data}
        def passed(name):result['checks'].append({'name':name,'status':'PASS'});print('PASS '+name,flush=True)
        try:
            daemon=subprocess.Popen(['pmuxd','--socket',os.environ['PMUX_SOCKET'],'--','/bin/bash','--noprofile','--norc'],stdout=(out/'daemon.log').open('w'),stderr=subprocess.STDOUT)
            wait(lambda:Path(os.environ['PMUX_SOCKET']).exists(),'daemon')
            cli('space','create','alpha','--session-name','alpha-1','--no-attach')
            spec=importlib.util.spec_from_file_location('ux',Path(__file__).with_name('host-ux-e2e.py'));ux=importlib.util.module_from_spec(spec);spec.loader.exec_module(ux)
            display=subprocess.Popen(['Xvfb','-displayfd','1','-screen','0','1920x1080x24','-nolisten','tcp','-noreset'],stdout=subprocess.PIPE,stderr=(out/'xvfb.log').open('w'))
            os.environ['DISPLAY']=':'+display.stdout.readline().decode().strip()
            config='font_px = 16.0\nspace_rail = "left"\nwindow_opacity = 1.0\nspace_startup = "restore"\n[keys]\nspace_settings = "ctrl+alt+p"\n'
            host=ux.Host('daily',['--attach-session',str(sessions()['alpha-1'][0])],config)
            wait(lambda:host.status()['space']=='alpha','initial Space')
            def capture(name):
                time.sleep(.6)
                return host.capture(name)
            def shell(command):ux.run('xdotool','type','--clearmodifiers','--delay','1',command);host.key('Return')
            def choose(prefix):
                def ready():
                    panel=host.status().get('space_details')
                    return panel if panel and not panel['loading'] and any(row.startswith(prefix+':') for row in panel['rows']) else None
                panel=wait(ready,'setting '+prefix);index=next(i for i,row in enumerate(panel['rows']) if row.startswith(prefix+':'))
                host.key('Home')
                for _ in range(index):host.key('Down')
                host.key('Return');time.sleep(.4)
            def open_space(name):
                cli('space','open',name,'--no-attach','--no-run');wait(lambda:host.status()['space']==name and not host.status()['space_open_pending'],'open '+name);time.sleep(1.3)
            def local_pids():
                state=host.status();ids={f'PaneId({x})' for x in state['local_terminal_panes']}
                return {p['local_child_pid'] for p in state['current_panes'] if p['pane_id'] in ids}
            def drag(x1,x2):
                ux.run('xdotool','mousemove','--window',host.wid,str(x1),'180');ux.run('xdotool','mousedown','1');ux.run('xdotool','mousemove','--window',host.wid,str(x2),'180');time.sleep(.3);ux.run('xdotool','mouseup','1');time.sleep(1.3)
            assert host.status()['space_rail_width_cols']==18
            drag(host.status()['space_rail_px'],260)
            wait(lambda:host.status()['space_rail_width_cols']==26,'dragged left rail')
            cli('space','rename','alpha','A deliberately long Space name')
            wait(lambda:host.status()['space']=='A deliberately long Space name','Space renamed')
            assert host.status()['space_rail_width_cols']==26
            capture('fixed-resizable-left-rail')
            passed('left-rail-resizes-and-rename-keeps-width')
            host.key('ctrl+alt+p');choose('Rail: right');host.key('Escape')
            wait(lambda:host.status()['space_rail_position']=='right','right rail')
            width=int(dict(line.split('=',1) for line in ux.run('xdotool','getwindowgeometry','--shell',host.wid).splitlines())['WIDTH'])
            drag(width-host.status()['space_rail_px'],width-220)
            wait(lambda:host.status()['space_rail_width_cols']==22,'dragged right rail')
            capture('resizable-right-rail')
            passed('right-rail-resizes-and-saves-preference')
            host.key('ctrl+alt+p');choose('Session names: blank');capture('blank-default-settings');host.key('Escape')
            before=sessions()
            host.key('ctrl+shift+e');wait(lambda:len(local_pids())==1,'default blank split')
            assert not host.status()['session_prompt_open'] and sessions()==before
            host.key('ctrl+shift+t');wait(lambda:len(local_pids())==2,'default blank tab')
            host.key('ctrl+alt+shift+n');wait(lambda:len(sessions())==len(before)+1,'explicit session shortcut')
            wait(lambda:not host.status()['session_prompt_open'] and host.status()['focused_session'],'explicit session attached')
            host.key('ctrl+alt+shift+t');wait(lambda:len(local_pids())==3,'explicit blank shortcut')
            capture('blank-and-session-shortcuts')
            passed('blank-default-and-explicit-session-shortcuts-skip-popup')
            work=out/'project-scratch';work.mkdir()
            shell('cd '+str(work)+'; export KEPT_MOVE_TOKEN=alive')
            time.sleep(.5)
            moved_pid=next(p['local_child_pid'] for p in host.status()['current_panes'] if p['pane_id']==f"PaneId({host.status()['focused_pane']})")
            cli('space','create','beta','--session-name','beta-1','--no-attach')
            wait(lambda:host.status()['space']=='beta','beta created')
            open_space('A deliberately long Space name')
            assert moved_pid in local_pids()
            # Invoke the pane's context menu, then its existing Move to Space action.
            pane,slot=host.status()['visible_pane_slots'][0]
            ux.run('xdotool','mousemove','--window',host.wid,str(slot[0]+40),str(slot[1]+60));ux.run('xdotool','click','3')
            wait(lambda:host.status()['context_menu'],'pane menu')
            host.key('Home')
            for _ in range(4):host.key('Down')
            capture('move-blank-context-menu');host.key('Return')
            wait(lambda:host.status()['space_picker_open'],'move picker');ux.run('xdotool','type','--clearmodifiers','beta');host.key('Return')
            wait(lambda:moved_pid not in local_pids(),'blank moved out')
            assert host.status()['space']=='A deliberately long Space name'
            before=sessions();open_space('beta');assert moved_pid in local_pids() and sessions()==before
            token=out/'move-token';shell('printf %s "$KEPT_MOVE_TOKEN" > '+str(token));wait(lambda:token.exists(),'moved process token');assert token.read_text()=='alive'
            capture('moved-blank-terminal')
            passed('context-move-preserves-local-process-directory-and-shell-state')
            open_space('A deliberately long Space name');before=sessions();host.key('ctrl+shift+o')
            wait(lambda:host.status()['terminal_switcher'] is not None,'terminal switcher');ux.run('xdotool','type','--clearmodifiers','project-scratch');time.sleep(.4);capture('terminal-switcher-search');host.key('Return')
            wait(lambda:host.status()['space']=='beta' and host.status()['focused_session'] is None,'switcher focused blank')
            assert moved_pid in local_pids() and sessions()==before
            host.key('ctrl+shift+o');wait(lambda:host.status()['terminal_switcher'] is not None,'switcher reopened');ux.run('xdotool','type','--clearmodifiers','alpha-1');host.key('Return')
            wait(lambda:host.status()['focused_session']==str(before['alpha-1'][0]),'switcher focused session')
            assert sessions()==before
            passed('switcher-finds-blank-and-managed-terminals-without-new-processes')
            # A narrow tab must keep branch and dirty status visible.
            repo=out/'repo';repo.mkdir();subprocess.run(['git','-C',str(repo),'init','-b','feature'],check=True,capture_output=True);(repo/'untracked').write_text('dirty')
            shell('cd '+str(repo));wait(lambda:any(x and x.endswith(':feature *') for x in host.status()['tab_git_labels']),'Git label')
            time.sleep(.6);capture('git-updates-without-hover')
            ux.run('xdotool','windowsize',host.wid,'660','600');time.sleep(.8);capture('compact-git-label')
            ux.run('xdotool','mousemove','--window',host.wid,'70','10');time.sleep(.3);capture('full-git-hover')
            host.key('ctrl+alt+p');choose('Rail: top');host.key('Escape')
            wait(lambda:host.status()['space_rail_position']=='top','top rail Git hover')
            ux.run('xdotool','mousemove','--window',host.wid,'70',str(host.status()['tab_strip_y']+10));capture('top-rail-git-hover')
            host.key('ctrl+alt+p');choose('Rail: right');host.key('Escape')
            ux.run('xdotool','windowsize',host.wid,'1006','606');time.sleep(.5)
            passed('compact-git-label-and-full-hover-render')
            host.key('ctrl+alt+p');choose('Restore blanks: off');capture('restore-blank-settings');host.key('Escape')
            config=(host.directory/'config.toml').read_text();assert 'restore_blank_terminals = true' in config
            open_space('beta');old_locals=local_pids();assert moved_pid in old_locals
            old_tabs=host.status()['tab_count'];before=sessions();time.sleep(2)
            host.stop();host=ux.Host('daily-restored',[],config)
            wait(lambda:host.status()['space']=='beta' and len(local_pids())==len(old_locals),'blank terminals restored')
            assert not (local_pids() & old_locals) and host.status()['tab_count']==old_tabs
            assert sessions()==before and host.status()['space_rail_width_cols']==22
            focused_pid=next(p['local_child_pid'] for p in host.status()['current_panes'] if p['pane_id']==f"PaneId({host.status()['focused_pane']})")
            assert Path('/proc/'+str(focused_pid)+'/cwd').resolve()==work
            capture('restored-blank-layout')
            passed('opt-in-restore-keeps-tabs-cwd-focus-and-managed-processes-with-fresh-local-shells')
            open_space('A deliberately long Space name');wait(lambda:len(local_pids())==2,'hidden Space recipes restored');capture('restored-hidden-space')
            passed('hidden-space-blank-layout-restores-on-first-open')
            host.key('ctrl+shift+o');wait(lambda:host.status()['terminal_switcher'] is not None,'stale switcher');ux.run('xdotool','type','--clearmodifiers','beta-1');cli('stop','beta-1');host.key('Return');time.sleep(.5)
            assert host.status()['space']=='A deliberately long Space name' and 'beta-1' not in sessions()
            capture('stale-switcher-target')
            passed('stale-switcher-target-does-not-switch-or-recreate-session')
            host.key('ctrl+alt+shift+t');wait(lambda:host.status()['focused_session'] is None,'direct blank tab for splits')
            for chord,blank in [('ctrl+alt+shift+e',True),('ctrl+alt+shift+d',True),('ctrl+alt+shift+r',False),('ctrl+alt+shift+b',False)]:
                count=len(host.status()['current_panes']);before=sessions()
                host.key(chord)
                wait(lambda:len(host.status()['current_panes'])==count+1 and not host.status()['session_prompt_open'],'direct split '+chord)
                if blank:assert sessions()==before
                else:wait(lambda:host.status()['focused_session'] is not None and len(sessions())==len(before)+1,'direct session split attached')
            capture('direct-split-shortcuts');passed('all-four-direct-split-shortcuts-skip-naming-popup')
            host.key('ctrl+alt+p');choose('Restore blanks: on');host.key('Escape');time.sleep(1.5)
            config=(host.directory/'config.toml').read_text();host.stop();host=ux.Host('daily-disabled',[],config)
            wait(lambda:host.status()['space']=='A deliberately long Space name','disabled restart');time.sleep(1.5);assert not local_pids()
            passed('disabling-blank-restore-does-not-resurrect-old-shells')
            result['status']='PASS'
        except Exception as error:
            if host:
                try:capture('failure');(out/'failure-status.json').write_text(json.dumps(host.status(),indent=2))
                except Exception:pass
            import traceback
            result.update(status='FAIL',error=str(error),traceback=traceback.format_exc());raise
        finally:
            if host:host.stop()
            if display:display.terminate();display.wait(timeout=5)
            if daemon:daemon.terminate();daemon.wait(timeout=5)
            (out/'result.json').write_text(json.dumps(result,indent=2)+'\n')
if __name__=='__main__':main()
