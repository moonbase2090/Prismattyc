#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Drive settings on an allocated macOS desktop; retain owned-window evidence.

Requires coordinator allocation, an unlocked desktop, and Accessibility access.
--check-desktop-only performs a read-only prerequisite check without launches.
This fixture is separate from the non-window painter and persistence probes.
"""
import argparse
import ctypes as C
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import time
import tomllib


def checked_window_status(data,host_pid,now_ms,after_ms=0):
    assert data['host_pid']==host_pid,'Render status belongs to another host'
    window=data['windows'][0]
    raster=window['last_raster'];stamp=raster['unix_ms']
    assert raster['present_succeeded'],'Last frame did not present'
    assert isinstance(stamp,int) and not isinstance(stamp,bool),'Raster timestamp must be integer milliseconds'
    assert now_ms-5000<=stamp<=now_ms+1000,'Last successful frame is stale or in the future'
    assert stamp>after_ms,'No successful frame after the native action'
    return window


class Desktop:
    def __init__(self):
        self.ax=C.CDLL('/System/Library/Frameworks/ApplicationServices.framework/ApplicationServices')
        self.cf=C.CDLL('/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation')
        C.CDLL('/System/Library/Frameworks/AppKit.framework/AppKit')
        objc=C.CDLL('/usr/lib/libobjc.A.dylib'); p=C.c_void_p
        objc.objc_getClass.argtypes=[C.c_char_p];objc.objc_getClass.restype=p
        objc.sel_registerName.argtypes=[C.c_char_p];objc.sel_registerName.restype=p
        self.cls=objc.objc_getClass;self.sel=objc.sel_registerName
        self.msg=C.CFUNCTYPE(p,p,p)(('objc_msgSend',objc))
        self.utf8=C.CFUNCTYPE(C.c_char_p,p,p)(('objc_msgSend',objc))
        self.running=C.CFUNCTYPE(p,p,p,C.c_int)(('objc_msgSend',objc))
        self.activate=C.CFUNCTYPE(C.c_bool,p,p,C.c_ulong)(('objc_msgSend',objc))
        for fn,args,result in [
            (self.ax.AXIsProcessTrusted,[],C.c_bool),
            (self.ax.AXUIElementCreateApplication,[C.c_int],p),
            (self.ax.AXUIElementCopyAttributeValue,[p,p,C.POINTER(p)],C.c_int),
            (self.ax.AXUIElementPerformAction,[p,p],C.c_int),
            (self.ax.CGEventCreateKeyboardEvent,[p,C.c_uint16,C.c_bool],p),
            (self.ax.CGEventSetFlags,[p,C.c_uint64],None),
            (self.ax.CGEventPostToPid,[C.c_int,p],None),
            (self.ax.CGWindowListCopyWindowInfo,[C.c_uint32,C.c_uint32],p),
            (self.cf.CFStringCreateWithCString,[p,C.c_char_p,C.c_uint32],p),
            (self.cf.CFStringGetCString,[p,p,C.c_long,C.c_uint32],C.c_bool),
            (self.cf.CFArrayGetCount,[p],C.c_long),
            (self.cf.CFArrayGetValueAtIndex,[p,C.c_long],p),
            (self.cf.CFDictionaryGetValue,[p,p],p),
            (self.cf.CFNumberGetValue,[p,C.c_int,p],C.c_bool),
            (self.cf.CFRelease,[p],None),
            (self.cf.CFRetain,[p],p),
            (self.cf.CFGetTypeID,[p],C.c_ulong),
            (self.cf.CFStringGetTypeID,[],C.c_ulong),
            (self.cf.CFBooleanGetValue,[p],C.c_bool),
        ]:
            fn.argtypes=args;fn.restype=result

    def string(self,text):
        return self.cf.CFStringCreateWithCString(None,text.encode(),0x08000100)

    def attr(self,node,key):
        name=self.string(key);value=C.c_void_p()
        try:
            return value.value if self.ax.AXUIElementCopyAttributeValue(node,name,C.byref(value))==0 else None
        finally:self.cf.CFRelease(name)

    def text(self,value):
        if not value or self.cf.CFGetTypeID(value)!=self.cf.CFStringGetTypeID():return ''
        buf=C.create_string_buffer(16384)
        return buf.value.decode() if self.cf.CFStringGetCString(value,buf,len(buf),0x08000100) else ''

    def prerequisite(self):
        if not self.ax.AXIsProcessTrusted():raise RuntimeError('Accessibility permission is required')
        workspace=self.msg(self.cls(b'NSWorkspace'),self.sel(b'sharedWorkspace'))
        front=self.msg(workspace,self.sel(b'frontmostApplication'))
        if not front:raise RuntimeError('No foreground desktop')
        ident=self.msg(front,self.sel(b'bundleIdentifier'))
        bundle=self.utf8(ident,self.sel(b'UTF8String')) if ident else b''
        name=self.msg(front,self.sel(b'localizedName'))
        name=self.utf8(name,self.sel(b'UTF8String')) if name else b''
        if bundle==b'com.apple.loginwindow' or name==b'loginwindow':raise RuntimeError('Unlocked logged-in desktop is required')

    def require_focused(self,pid):
        self.prerequisite()
        app=self.ax.AXUIElementCreateApplication(pid)
        front=self.attr(app,'AXFrontmost');focused=self.attr(app,'AXFocusedWindow')
        try:
            assert front and self.cf.CFBooleanGetValue(front),'Owned host is not frontmost'
            assert focused,'Owned host has no focused window'
        finally:
            if front:self.cf.CFRelease(front)
            if focused:self.cf.CFRelease(focused)
            self.cf.CFRelease(app)

    def key(self,pid,code,flags=0):
        self.require_focused(pid)
        for pressed in [True,False]:
            event=self.ax.CGEventCreateKeyboardEvent(None,code,pressed)
            if not event:raise RuntimeError('Keyboard event allocation failed')
            try:self.ax.CGEventSetFlags(event,flags);self.ax.CGEventPostToPid(pid,event)
            finally:self.cf.CFRelease(event)
        for mask,modifier in [(1<<17,56),(1<<18,59),(1<<19,58),(1<<20,55)]:
            if flags&mask:
                event=self.ax.CGEventCreateKeyboardEvent(None,modifier,False)
                try:self.ax.CGEventSetFlags(event,0);self.ax.CGEventPostToPid(pid,event)
                finally:self.cf.CFRelease(event)
        time.sleep(.15)

    def press_row(self,pid,fragment):
        app=self.ax.AXUIElementCreateApplication(pid)
        found=[]
        def visit(node,depth=0):
            if depth>16:return
            title=self.attr(node,'AXTitle')
            try:
                if fragment in self.text(title):found.append(self.cf.CFRetain(node))
            finally:
                if title:self.cf.CFRelease(title)
            children=self.attr(node,'AXChildren')
            if children:
                try:
                    for index in range(self.cf.CFArrayGetCount(children)):
                        visit(self.cf.CFArrayGetValueAtIndex(children,index),depth+1)
                finally:self.cf.CFRelease(children)
        # Retain nodes while their parents are being traversed through the app tree.
        try:
            visit(app)
            assert len(found)==1, f'Expected one accessible row containing {fragment!r}, found {len(found)}'
            action=self.string('AXPress')
            try:
                self.require_focused(pid)
                assert self.ax.AXUIElementPerformAction(found[0],action)==0, 'Row AXPress failed'
            finally:self.cf.CFRelease(action)
        finally:
            for node in found:self.cf.CFRelease(node)
            self.cf.CFRelease(app)
        time.sleep(.25)

    def capture(self,pid,destination):
        windows=self.ax.CGWindowListCopyWindowInfo(1|16,0)
        recorded=[]
        try:
            for index in range(self.cf.CFArrayGetCount(windows)):
                row=self.cf.CFArrayGetValueAtIndex(windows,index)
                def number(name):
                    key=self.string(name)
                    try:value=self.cf.CFDictionaryGetValue(row,key)
                    finally:self.cf.CFRelease(key)
                    result=C.c_long();assert self.cf.CFNumberGetValue(value,4,C.byref(result));return result.value
                if number('kCGWindowOwnerPID')!=pid:continue
                window=number('kCGWindowNumber')
                output=destination.with_name(destination.stem+f'-{window}.png')
                subprocess.run(['/usr/sbin/screencapture','-x','-o','-l',str(window),str(output)],check=True,capture_output=True)
                recorded.append({'owner_pid':pid,'window_id':window,'file':output.name})
        finally:self.cf.CFRelease(windows)
        assert recorded, 'No owned-window capture'
        return recorded


def stop(process):
    if process is not None and process.poll() is None:
        process.terminate()
        try:process.wait(timeout=5)
        except subprocess.TimeoutExpired:process.kill();process.wait(timeout=5)


def finish_evidence(root,result,commands,captures,host,daemon,clipboard):
    errors=[]
    for label,operation in [
        ('host cleanup',lambda:stop(host)),
        ('daemon cleanup',lambda:stop(daemon)),
        ('clipboard restore',lambda:subprocess.run(['/usr/bin/pbcopy'],input=clipboard,check=True)),
    ]:
        try:operation()
        except Exception as error:errors.append(f'{label}: {error}')
    result.update(host_reaped=host is None or host.poll() is not None,daemon_reaped=daemon is None or daemon.poll() is not None)
    if errors:result.update(status='FAIL',cleanup_errors=errors)
    for filename,value in [('result.json',result),('commands.json',commands),('captures.json',captures)]:
        (root/filename).write_text(json.dumps(value,indent=2)+'\n')
    if errors:raise RuntimeError('; '.join(errors))


def run_case(args,desktop,style):
    root=args.out/style;root.mkdir()
    for name in ['home','config','data','state','runtime']: (root/name).mkdir(mode=0o700)
    config=root/'config.toml'
    config.write_text(f'chrome_style="{style}"\nsplash=false\nstart_at_login=false\nautomatic_update_checks=false\ninstall_agent_skills=false\nspace_startup="restore"\n[a11y]\nos_tree=true\n[keys]\nspace_settings="f6"\nnew_tab=["left","right","up","down"]\n')
    wrapper=root/'shell';wrapper.write_text('#!/bin/sh\nexport PS1="proof$ "\nexec /bin/sh\n');wrapper.chmod(0o755)
    env={key:value for key,value in os.environ.items() if not key.startswith(('PMUX','PRISMATTYC_','XDG_'))}
    env.update(HOME=str(root/'home'),SHELL=str(wrapper),PATH=f'{args.bins}:/usr/bin:/bin:/usr/sbin:/sbin',PMUX_SOCKET=str(root/'runtime/pmux.sock'),XDG_RUNTIME_DIR=str(root/'runtime'),XDG_CONFIG_HOME=str(root/'config'),XDG_DATA_HOME=str(root/'data'),XDG_STATE_HOME=str(root/'state'),PRISMATTYC_CONFIG=str(config),PRISMATTYC_NO_AGENT_SKILLS='1',PRISMATTYC_DUMP_PRESENT=str(root/'present.png'))
    daemon=host=None;commands=[];captures=[];result={'style':style,'revision':args.revision,'status':'FAIL'}
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
    def identities():
        with socket.socket(socket.AF_UNIX) as stream:
            stream.settimeout(5);stream.connect(env['PMUX_SOCKET'])
            stream.sendall(b'{"version":1,"request_id":1,"type":"snapshot"}\n')
            snapshot=json.loads(stream.makefile().readline())['response']['snapshot']
        return sorted((session['id'],window['id'],pane['id'],pane['child_pid']) for session in snapshot['sessions'] for window in session['windows'] for pane in window['panes'])
    def pane_lines():return [line.rstrip() for line in cli('save-buffer','sample-1','-').splitlines() if line.strip()]
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
    try:
        with (root/'daemon.log').open('w') as log:
            daemon=subprocess.Popen([str(args.bins/'pmuxd'),'--socket',env['PMUX_SOCKET'],'--',str(wrapper)],env=env,cwd=root,stdout=log,stderr=log)
        wait(lambda:Path(env['PMUX_SOCKET']).exists(),'daemon socket');cli('space','create','sample','--no-attach')
        cli('send','sample-1',"printf '0O 1Il {} [] != ->\\n'",'--enter')
        with (root/'host.log').open('w') as log:
            host=subprocess.Popen([str(args.bins/'prismattyc-host'),'--no-splash','--attach-session','sample-1'],env=env,cwd=root,stdout=log,stderr=log)
        wait(lambda:status().get('space')=='sample','owned host')
        desktop.prerequisite()
        running=desktop.running(desktop.cls(b'NSRunningApplication'),desktop.sel(b'runningApplicationWithProcessIdentifier:'),host.pid)
        assert running and desktop.activate(running,desktop.sel(b'activateWithOptions:'),2),'Owned host activation failed'
        wait(lambda:status()['focused'],'focused host')
        before=identities();wait(lambda:any('0O 1Il {} [] != ->' in line for line in pane_lines()),'neutral comparison text')
        before_text=pane_lines();capture('default-font',False)
        key(97) # F6 opens legacy settings.
        key(115) # Home.
        if style=='graphite':key(125);key(125) # Skip layout choices.
        key(36)
        wait(lambda:saved().get('settings_panel') is True,'full settings enable');capture('enabled',True)
        key(125);key(125) # Enter Appearance settings and select tab_strip.
        key(36)
        wait(lambda:saved().get('tab_strip')=='always','Enter cycles and persists a finite choice')
        capture('choice-cycled-with-enter',True)
        key(123)
        wait(lambda:saved().get('tab_strip','auto')=='auto','Left cycles to the previous choice')
        key(124)
        wait(lambda:saved().get('tab_strip')=='always','Right cycles to the next choice')
        capture('choice-cycled-with-arrows',True)
        key(36,1<<17) # Shift+Enter opens the detailed choice editor.
        capture('choice-editor',True)
        press_row(': Reset / inherit ')
        wait(lambda:'tab_strip' not in saved(),'choice reset')
        capture('choice-reset',True)
        key(53);key(53) # Close the editor and return to section navigation.
        key(124);key(123);key(124) # Fonts, Appearance, Fonts, without modifiers.
        key(125);key(125);key(125);key(36);key(36) # Font size, exact edit.
        subprocess.run(['/usr/bin/pbcopy'],input=b'18.5',check=True);key(9,1<<20);key(36)
        press_row(': Save ')
        wait(lambda:saved().get('font_px')==18.5,'literal font size save');capture('saved-size',True)
        key(36);press_row(': Reset / inherit ')
        wait(lambda:'font_px' not in saved(),'font size reset');capture('reset-size',True)
        key(53);key(53);wait(lambda:not status().get('context_menu'),'settings closed')
        assert identities()==before,'Modal arrow bindings changed live sessions'
        assert pane_lines()==before_text,'Modal navigation or text reached the PTY'
        for identity,label in [('bundled:ibm-plex-mono','IBM Plex Mono'),('bundled:commit-mono','Commit Mono')]:
            key(97);key(124);key(125);key(36)
            press_row(label+' (bundled)');press_row(': Save ')
            wait(lambda:saved().get('font')==identity,'font selection')
            wait(lambda:('font '+label+' (') in (root/'host.log').read_text(),'actual primary font load')
            key(53);key(53);time.sleep(1);capture(identity.split(':')[1],False)
        key(97);key(125);key(36) # Appearance, first setting, exact bool editor.
        press_row(': Reset / inherit ')
        wait(lambda:'settings_panel' not in saved(),'gate reset')
        capture('legacy-restored',True)
        result.update(status='PASS',sessions='preserved',choice_cycle='enter-arrow-directions-shift-enter-reset',font_px='saved-and-reset',fonts='selected-through-native-accessibility',gate='enabled-and-reset')
    except Exception as error:
        result['error']=str(error)
        raise
    finally:
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
    results=[run_case(args,desktop,style) for style in ['classic','graphite']]
    (args.out/'result.json').write_text(json.dumps({'revision':args.revision,'status':'PASS','cases':results},indent=2)+'\n')
    return 0


if __name__=='__main__':sys.exit(main())
