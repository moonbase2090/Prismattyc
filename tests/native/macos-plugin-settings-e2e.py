#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Drive plug-in Settings on an allocated macOS desktop; retain owned-window evidence.

Requires coordinator allocation, an unlocked desktop, and Accessibility access.
--check-desktop-only performs a read-only prerequisite check without launches.
This fixture is separate from the non-window painter and persistence probes.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import time
import tomllib
import threading
import select


# Share the established permission, focus, owned-window, and cleanup boundary.
import importlib.util
helper_path=Path(__file__).with_name('macos-settings-e2e.py')
helper_spec=importlib.util.spec_from_file_location('native_settings',helper_path)
helper=importlib.util.module_from_spec(helper_spec)
helper_spec.loader.exec_module(helper)
Desktop=helper.Desktop
checked_window_status=helper.checked_window_status
finish_evidence=helper.finish_evidence


class ListProxy:
    """Forward a real daemon, withholding only plug-in List replies."""
    def __init__(self,path,upstream):
        self.upstream=upstream;self.block=False;self.closed=False;self.events=[];self.streams=[]
        self.listener=socket.socket(socket.AF_UNIX);self.listener.bind(str(path));self.listener.listen();self.listener.settimeout(.2)
        self.thread=threading.Thread(target=self.serve,daemon=True);self.thread.start()
    def serve(self):
        while not self.closed:
            try:front,_=self.listener.accept()
            except socket.timeout:continue
            except OSError:break
            self.streams.append(front);threading.Thread(target=self.relay,args=(front,),daemon=True).start()
    def relay(self,front):
        back=socket.socket(socket.AF_UNIX);self.streams.append(back)
        try:
            back.connect(str(self.upstream));pending=b''
            while not self.closed:
                ready,_,_=select.select([front,back],[],[],.2)
                for source in ready:
                    data=source.recv(65536)
                    if not data:return
                    if source is back:front.sendall(data);continue
                    pending+=data
                    while b'\n' in pending:
                        line,pending=pending.split(b'\n',1);request=json.loads(line)
                        if self.block and request.get('type')=='plugin' and request.get('operation',{}).get('operation')=='list':
                            event={'request_id':request['request_id'],'blocked':True,'cancelled':False};self.events.append(event)
                            while self.block and not self.closed:
                                if select.select([front],[],[],.1)[0]:
                                    if not front.recv(1):event['cancelled']=True;return
                                    raise AssertionError('Unexpected bytes while List is blocked')
                            # A closed page must have cancelled before the block is released.
                            return
                        back.sendall(line+b'\n')
        except (BrokenPipeError,ConnectionResetError,OSError):pass
        finally:front.close();back.close()
    def close(self):
        self.closed=True;self.listener.close()
        for stream in self.streams:
            try:stream.shutdown(socket.SHUT_RDWR);stream.close()
            except OSError:pass
        self.thread.join(timeout=2)


def row_titles(desktop,pid):
    app=desktop.ax.AXUIElementCreateApplication(pid);titles=[]
    def visit(node,depth=0):
        if depth>16:return
        title=desktop.attr(node,'AXTitle')
        if title:
            try:titles.append(desktop.text(title))
            finally:desktop.cf.CFRelease(title)
        children=desktop.attr(node,'AXChildren')
        if children:
            try:
                for i in range(desktop.cf.CFArrayGetCount(children)):visit(desktop.cf.CFArrayGetValueAtIndex(children,i),depth+1)
            finally:desktop.cf.CFRelease(children)
    try:visit(app)
    finally:desktop.cf.CFRelease(app)
    return titles


def run_case(args,desktop,style):
    root=args.out/style;root.mkdir()
    for name in ['home','config','data','state','runtime']: (root/name).mkdir(mode=0o700)
    config=root/'config.toml'
    config.write_text(f'chrome_style="{style}"\nsplash=false\nstart_at_login=false\nautomatic_update_checks=false\ninstall_agent_skills=false\nspace_startup="restore"\n[a11y]\nos_tree=true\n[keys]\nspace_settings="ctrl+alt+f6"\n')
    wrapper=root/'shell';wrapper.write_text('#!/bin/sh\nexport PS1="proof$ "\nexec /bin/sh\n');wrapper.chmod(0o755)
    env={key:value for key,value in os.environ.items() if not key.startswith(('PMUX','PRISMATTYC_','XDG_'))}
    env.update(HOME=str(root/'home'),SHELL=str(wrapper),PATH=f'{args.bins}:/usr/bin:/bin:/usr/sbin:/sbin',PMUX_SOCKET=str(root/'runtime/pmux.sock'),XDG_RUNTIME_DIR=str(root/'runtime'),XDG_CONFIG_HOME=str(root/'config'),XDG_DATA_HOME=str(root/'data'),XDG_STATE_HOME=str(root/'state'),PRISMATTYC_CONFIG=str(config),PRISMATTYC_NO_AGENT_SKILLS='1',PRISMATTYC_DUMP_PRESENT=str(root/'present.png'))
    # This proof drives the manual install and enable path, so start with no default plug-ins.
    env['PRISMATTYC_DEFAULT_PLUGINS']='0'
    daemon=host=None;proxy=None;commands=[];captures=[];result={'style':style,'revision':args.revision,'status':'FAIL'}
    clipboard=subprocess.check_output(['/usr/bin/pbpaste'])
    last_action_ms=0
    def cli(*argv):
        proc=subprocess.run([str(args.bins/'pmux'),*map(str,argv)],env=env,capture_output=True,text=True,timeout=20)
        commands.append({'args':argv,'exit':proc.returncode,'stdout':proc.stdout,'stderr':proc.stderr})
        assert proc.returncode==0,proc.stderr
        return proc.stdout
    def wait(probe,label):
        deadline=time.monotonic()+20
        while time.monotonic()<deadline:
            if host:assert host.poll() is None,f'Host exited during {label}'
            try:
                value=probe()
                if value:return value
            except (FileNotFoundError,ValueError,KeyError,AssertionError):pass
            time.sleep(.1)
        raise AssertionError('Timeout: '+label)
    def status(after_ms=0):
        assert host.poll() is None,'Owned host has exited'
        return checked_window_status(json.loads(cli('render-status','--json')),host.pid,int(time.time()*1000),after_ms)
    def snapshot():
        with socket.socket(socket.AF_UNIX) as stream:
            stream.settimeout(5);stream.connect(env['PMUX_SOCKET'])
            stream.sendall(b'{"version":1,"request_id":1,"type":"snapshot"}\n')
            return json.loads(stream.makefile().readline())['response']['snapshot']
    def identities():
        data=snapshot()
        return sorted((session['id'],window['id'],pane['id'],pane['child_pid']) for session in data['sessions'] for window in session['windows'] for pane in window['panes'])
    def pane_lines():return [line.rstrip() for line in cli('save-buffer',str(sample_pane),'-').splitlines() if line.strip()]
    def saved():return tomllib.loads(config.read_text())
    def key(code,flags=0):
        nonlocal last_action_ms
        assert status()['focused'],'Owned host must be focused before native input'
        last_action_ms=int(time.time()*1000)
        desktop.key(host.pid,code,flags)
    def press_row(fragment):
        nonlocal last_action_ms
        assert status()['focused'],'Owned host must be focused before Accessibility input'
        last_action_ms=int(time.time()*1000)
        desktop.press_row(host.pid,fragment)
    def capture(label,overlay):
        time.sleep(.3)
        window=wait(lambda:status(last_action_ms),'fresh frame after action for '+label)
        assert window['focused'],'Owned window lost focus before capture'
        assert bool(window.get('context_menu'))==overlay,'Unexpected overlay state for '+label
        captures.extend(desktop.capture(host.pid,root/(label+'.png')))
        shutil.copy2(config,root/(label+'.toml'))
        shutil.copy2(root/'present.png',root/(label+'-framebuffer.png'))
    real_socket=root/'runtime/real.sock'
    def truth():return json.loads(cli('--socket',str(real_socket),'plugin','list'))
    def state():return next((x for x in truth()['states'] if x['id']=='org.moonbase.activity-monitor'),None)
    def has_row(fragment):return any(fragment in t for t in row_titles(desktop,host.pid))
    def choose(fragment):
        wait(lambda:has_row(fragment),'accessible row '+fragment);press_row(fragment)
    def open_plugins():
        key(97,(1<<18)|(1<<19));choose(': Plug-ins ')
    def open_monitor():
        choose(': org.moonbase.activity-monitor ');wait(lambda:has_row(': Version 1.0.0 '),'monitor detail')
    try:
        proxy=ListProxy(Path(env['PMUX_SOCKET']),real_socket)
        with (root/'daemon.log').open('w') as log:
            daemon=subprocess.Popen([str(args.bins/'pmuxd'),'--socket',str(real_socket),'--',str(wrapper)],env=env,cwd=root,stdout=log,stderr=log)
        wait(lambda:real_socket.exists(),'daemon socket');cli('space','create','sample','--no-attach')
        sample=next(x for x in snapshot()['sessions'] if x['name']=='sample-1')
        sample_pane=sample['windows'][0]['panes'][0]['id']
        cli('send',str(sample_pane),"printf '0O 1Il {} [] != ->\\n'",'--enter')
        with (root/'host.log').open('w') as log:
            host=subprocess.Popen([str(args.bins/'prismattyc-host'),'--no-splash','--attach-session','sample-1'],env=env,cwd=root,stdout=log,stderr=log)
        wait(lambda:status().get('space')=='sample','owned host')
        assert 'ignoring config' not in (root/'host.log').read_text(),'Fixture configuration rejected'
        desktop.prerequisite()
        running=desktop.running(desktop.cls(b'NSRunningApplication'),desktop.sel(b'runningApplicationWithProcessIdentifier:'),host.pid)
        assert running and desktop.activate(running,desktop.sel(b'activateWithOptions:'),2),'Owned host activation failed'
        wait(lambda:status()['focused'],'focused host')
        before=identities();wait(lambda:any('0O 1Il {} [] != ->' in line for line in pane_lines()),'neutral comparison text')
        before_text=pane_lines();capture('default-font',False)
        assert truth()['installed']==[],'Private store is not empty'
        open_plugins();open_monitor();choose(': Version 1.0.0 ')
        capture('version-review',True)
        choose(': Install version 1.0.0 ')
        installed=wait(lambda:next((x for x in truth()['installed'] if x['id']=='org.moonbase.activity-monitor'),None),'installed package')
        assert installed['version']=='1.0.0' and installed['digest'],'Missing exact installed identity'
        current=state();assert current is None or (not current['enabled'] and not current['grants']),'Install granted access'
        choose(': Approve activity from local ')
        wait(lambda:any('Selected for the next Enable' in x for x in row_titles(desktop,host.pid)),'draft selected')
        current=state();assert current is None or (not current['enabled'] and not current['grants']),'Draft applied effective grants'
        capture('installed-disabled-draft',True)
        choose(': Enable version 1.0.0 ')
        current=wait(lambda:(v if (v:=state()) and v['enabled'] else None),'enabled exact version')
        assert current['grants']==['activity.read:local'],current
        assert current['selected']==installed,(current,installed)
        capture('enabled',True)
        choose(': Disable plug-in ')
        wait(lambda:(v:=state()) and not v['enabled'] and v['grants']==[] and v['observed']=='stopped','disabled and revoked')
        capture('disabled',True)
        key(53);key(53);wait(lambda:not status().get('context_menu'),'closed')
        open_plugins();open_monitor()
        wait(lambda:any('Not selected' in x for x in row_titles(desktop,host.pid)),'draft reset on reopening')
        capture('reopened-disabled',True)
        key(53);key(53);wait(lambda:not status().get('context_menu'),'closed before blocked request')
        proxy.block=True
        for attempt in range(2):
            open_plugins();wait(lambda:len(proxy.events)==attempt+1,'blocked List accepted')
            wait(lambda:any('Loading plug-ins' in x for x in row_titles(desktop,host.pid)),'loading row')
            capture(f'blocked-{attempt+1}',True)
            key(53);key(53)
            wait(lambda:not status(last_action_ms).get('context_menu'),'native close while List blocked')
            wait(lambda:proxy.events[attempt]['cancelled'],'pending socket cancelled')
            capture(f'closed-blocked-{attempt+1}',False)
            assert proxy.block,'RPC was released before close'
        proxy.block=False
        open_plugins();open_monitor()
        wait(lambda:any('Not selected' in x for x in row_titles(desktop,host.pid)),'fresh detail after blocked reopen')
        current=state();assert not current['enabled'] and current['grants']==[] and current['observed']=='stopped',current
        capture('fresh-after-cancellation',True)
        key(53);key(53);wait(lambda:not status().get('context_menu'),'final close')
        assert 'settings_panel' not in saved(),'Legacy entry changed the full-settings preference'
        assert identities()==before,'Modal input changed session, window, pane, or child PID'
        assert pane_lines()==before_text,'Modal input reached the PTY'
        result.update(status='PASS',sample_pane=sample_pane,identities_before=before,identities_after=identities(),pty_before=before_text,pty_after=pane_lines(),sessions='preserved',pty='preserved',install='disabled-without-grants',enable='exact-version-digest-and-local-grant',disable='stopped-and-grants-revoked',reopen='draft-reset',blocked_rpc=proxy.events,gate='remained-absent')
    except Exception as error:
        result['error']=str(error)
        if host and host.poll() is None:
            result['ax_titles']=row_titles(desktop,host.pid)
            try:captures.extend(desktop.capture(host.pid,root/'failure.png'))
            except Exception as capture_error:result['capture_error']=str(capture_error)
        raise
    finally:
        if proxy:proxy.close()
        finish_evidence(root,result,commands,captures,host,daemon,clipboard)
    return result


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bins',type=Path,required=True);parser.add_argument('--out',type=Path,required=True)
    parser.add_argument('--revision',required=True);parser.add_argument('--check-desktop-only',action='store_true')
    args=parser.parse_args();args.bins=args.bins.resolve();args.out=args.out.resolve();args.out.mkdir(parents=True,exist_ok=False)
    result={'revision':args.revision,'status':'BLOCKED','host_started':False,'daemon_started':False}
    try:
        assert sys.platform=='darwin','Requires macOS'
        desktop=Desktop();desktop.prerequisite()
    except Exception as error:
        result['prerequisite']=str(error);(args.out/'result.json').write_text(json.dumps(result,indent=2)+'\n');print(json.dumps(result));return 2
    if args.check_desktop_only:
        result['status']='AVAILABLE';(args.out/'result.json').write_text(json.dumps(result,indent=2)+'\n');print(json.dumps(result));return 0
    for binary in ['prismattyc-host','pmux','pmuxd']:
        version=subprocess.check_output([str(args.bins/binary),'--version'],text=True)
        assert args.revision[:12] in version,(binary,version)
    shutil.copy2(__file__,args.out/'fixture-script.py')
    shutil.copy2(helper_path,args.out/'macos-settings-e2e.py')
    results=[run_case(args,desktop,style) for style in ['classic','graphite']]
    (args.out/'result.json').write_text(json.dumps({'revision':args.revision,'status':'PASS','cases':results},indent=2)+'\n')
    return 0


if __name__=='__main__':sys.exit(main())
