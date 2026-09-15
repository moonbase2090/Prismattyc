#!/usr/bin/env python3
"""Isolated native restart, message UI, MCP lifecycle, and optional final soak."""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import select
import sys
sys.path.insert(0,str(Path(__file__).resolve().parent))
import importlib
atspi=importlib.import_module("polish-atspi")
import signal
import socket
import subprocess
import tempfile
import time

p=argparse.ArgumentParser(description=__doc__)
p.add_argument('--bins',type=Path,required=True)
p.add_argument('--out',type=Path,required=True)
p.add_argument('--soak-seconds',type=int,default=0)
p.add_argument('--atspi-only',action='store_true')
a=p.parse_args(); bins=a.bins.resolve(); out=a.out.resolve();out.mkdir(parents=True,exist_ok=False)
records=[]

def record(name,**data):
    records.append(dict(check=name,**data));print('PASS '+name,flush=True)
    (out/'result.json').write_text(json.dumps(dict(status='RUNNING',records=records),indent=2))

def wait(fn,label,seconds=12):
    deadline=time.monotonic()+seconds
    while time.monotonic()<deadline:
        try:
            result=fn()
            if result:return result
        except (OSError,ValueError,KeyError,subprocess.CalledProcessError):pass
        time.sleep(.05)
    raise AssertionError('timeout: '+label)

with tempfile.TemporaryDirectory(prefix='pmux-polish-') as runtime:
    runtime=Path(runtime)
    for key in list(os.environ):
        if key.startswith(('PMUX_','PRISMATTYC_')) or key in ('WAYLAND_DISPLAY','XAUTHORITY'):
            os.environ.pop(key,None)
    os.environ.update(PATH=str(bins)+os.pathsep+os.environ['PATH'],HOME=str(runtime),
        XDG_CONFIG_HOME=str(runtime/'config'),XDG_DATA_HOME=str(runtime/'data'),
        XDG_STATE_HOME=str(runtime/'state'),XDG_RUNTIME_DIR=str(runtime),
        PMUX_SOCKET=str(runtime/'pmux.sock'),HOST_UX_OUT=str(out),HOST_UX_NO_WM='1',WINIT_UNIX_BACKEND='x11',SHELL='/bin/bash')
    display_log=(out/'display.log').open('w')
    xvfb=subprocess.Popen(['Xvfb','-displayfd','1','-screen','0','1920x1080x24','-nolisten','tcp'],stdout=subprocess.PIPE,stderr=display_log,start_new_session=True)
    assert select.select([xvfb.stdout],[],[],8)[0], 'display startup timed out'
    os.environ['DISPLAY']=':'+xvfb.stdout.readline().decode().strip()
    log=(out/'daemon.log').open('w'); daemon=None;host=None;mcp=None
    a11y_log=(out/'a11y-bus.log').open('w')
    a11y_bus=subprocess.Popen(['dbus-daemon','--config-file=/usr/share/defaults/at-spi2/accessibility.conf','--print-address=1','--nofork'],stdout=subprocess.PIPE,stderr=a11y_log,start_new_session=True)
    assert select.select([a11y_bus.stdout],[],[],5)[0], 'private accessibility bus startup timed out'
    os.environ['AT_SPI_BUS_ADDRESS']=a11y_bus.stdout.readline().decode().strip()
    registry=subprocess.Popen(['/usr/lib/at-spi2-registryd'],env=dict(os.environ,DBUS_SESSION_BUS_ADDRESS=os.environ['AT_SPI_BUS_ADDRESS']),stdout=a11y_log,stderr=a11y_log,start_new_session=True)
    atspi_address=None
    try:atspi_address=atspi.enable()
    except (subprocess.CalledProcessError,subprocess.TimeoutExpired) as error:
        (out/'atspi-unavailable.txt').write_text(str(error))
    def cli(*args):
        return subprocess.check_output(['pmux',*map(str,args)],text=True,timeout=30).strip()
    def control(request):
        with socket.socket(socket.AF_UNIX) as sock:
            sock.settimeout(3);sock.connect(os.environ['PMUX_SOCKET'])
            sock.sendall((json.dumps(dict(version=1,request_id=1,**request))+'\n').encode())
            value=json.loads(sock.makefile().readline())
            assert 'response' in value,value
            return value['response']
    def snapshot():
        return control(dict(type='snapshot'))['snapshot']
    def children(pid):
        return Path(f'/proc/{pid}/task/{pid}/children').read_text().split()
    def rpc(value):
        mcp.stdin.write(json.dumps(value)+'\n');mcp.stdin.flush()
        if 'id' not in value:return
        assert select.select([mcp.stdout],[],[],8)[0], 'MCP response timeout'
        result=json.loads(mcp.stdout.readline());assert result['id']==value['id'];return result
    try:
        daemon=subprocess.Popen(['pmuxd','--socket',os.environ['PMUX_SOCKET'],'--','/bin/bash','--noprofile','--norc'],stdout=log,stderr=log,start_new_session=True)
        wait(lambda:Path(os.environ['PMUX_SOCKET']).exists(),'daemon socket')
        cli('space','create','polish-a','--no-attach');cli('space','create','polish-b','--no-attach')
        data=snapshot();session=next(s for s in data['sessions'] if s['name']=='polish-a-1')
        pane=session['windows'][0]['panes'][0];original=(pane['id'],pane['child_pid'])
        spec=importlib.util.spec_from_file_location('host_ux',Path(__file__).with_name('host-ux-e2e.py'))
        ux=importlib.util.module_from_spec(spec);spec.loader.exec_module(ux)
        config='font_px = 14.0\nspace_rail = "top"\nspace_startup = "restore"\n[keys]\nupdate_restart = "ctrl+alt+u"\nagent_messages = "ctrl+alt+m"\nblank_split_right = "ctrl+alt+b"\nclose_tab = "ctrl+alt+w"\n'
        host=ux.Host('native',['--attach-session',str(session['id'])],config)
        wait(lambda:json.loads(cli('restart','--host','--plan'))['host_pids'],'host registration')
        host.key('ctrl+alt+u');wait(lambda:host.status().get('space_details'),'maintenance menu')
        host.capture('update-restart')
        if atspi_address:
            atspi.run('--session','--dest','org.a11y.Bus','--object-path','/org/a11y/bus','--method','org.freedesktop.DBus.Properties.Set','org.a11y.Status','ScreenReaderEnabled','<false>')
            atspi.run('--session','--dest','org.a11y.Bus','--object-path','/org/a11y/bus','--method','org.freedesktop.DBus.Properties.Set','org.a11y.Status','ScreenReaderEnabled','<true>')
            try:
                nodes=wait(lambda: (lambda nodes:nodes if any('Restart MCP adapters' in n['name'] for n in nodes) else None)(atspi.tree(atspi_address)),'native AT-SPI maintenance rows',seconds=6)
                (out/'atspi-maintenance.json').write_text(json.dumps(nodes,indent=2));record('native-atspi-maintenance-rows')
            except (AssertionError,subprocess.CalledProcessError,subprocess.TimeoutExpired) as error:
                (out/'atspi-unavailable.txt').write_text(str(error))
                try:(out/'atspi-tree.json').write_text(json.dumps(atspi.tree(atspi_address),indent=2))
                except Exception as error:(out/'atspi-tree-error.txt').write_text(str(error))
        if (out/'atspi-maintenance.json').exists():
            node=next(n for n in nodes if 'Installed and running versions' in n['name'])
            result=atspi.run('--address',atspi_address,'--dest',node['bus'],'--object-path',node['path'],'--method','org.a11y.atspi.Action.DoAction','0')
            assert 'true' in result,result
            wait(lambda:any('Installed pmux:' in n['name'] for n in atspi.tree(atspi_address)),'native versions result',seconds=15)
            host.capture('component-versions');record('native-maintenance-action')
            result_rows=atspi.tree(atspi_address)
            back=next(n for n in result_rows if 'Back to update and restart' in n['name'])
            atspi.run('--address',atspi_address,'--dest',back['bus'],'--object-path',back['path'],'--method','org.a11y.atspi.Action.DoAction','0')
            wait(lambda:any('Restart MCP adapters' in n['name'] for n in atspi.tree(atspi_address)),'maintenance Back action')
            record('native-maintenance-back-action')
        if a.atspi_only:
            assert (out/'atspi-maintenance.json').exists(), 'native accessibility tree unavailable'
            (out/'result.json').write_text(json.dumps(dict(status='PASS',records=records),indent=2))
            sys.exit(0)
        host.key('Escape')
        assert host.status()['space']=='polish-a',host.status()
        before_status=host.status()
        before=host.process.pid;result=json.loads(cli('restart','--host'))
        assert any(c.get('response',{}).get('status')=='restarted' for c in result['components']),result
        host.wid=wait(lambda:ux.run('xdotool','search','--onlyvisible','--pid',str(before)),'replacement window').splitlines()[-1]
        ux.run('xdotool','windowfocus','--sync',host.wid)
        wait(lambda:host.status()['space']=='polish-a','restored Space')
        host.capture('after-host-restart')
        (out/'host-before.json').write_text(json.dumps(before_status,indent=2))
        (out/'host-after.json').write_text(json.dumps(host.status(),indent=2))
        after=next(s for s in snapshot()['sessions'] if s['id']==session['id'])['windows'][0]['panes'][0]
        assert (after['id'],after['child_pid'])==original
        record('host-restart-keeps-session-and-child',receipt=result)
        deferred=json.loads(cli('restart','--daemon'));assert deferred['components'][0]['status']=='deferred'
        assert daemon.poll() is None;record('daemon-restart-defers-live-sessions')
        time.sleep(1)
        receipt=json.loads(cli('pane-write',pane['id'],'--text','printf POLISH_NATIVE','--submit','enter','--json'))
        assert receipt['status']=='queued';time.sleep(.9)
        host.key('ctrl+alt+m');wait(lambda:host.status().get('terminal_switcher'),'message view');host.capture('agent-messages')
        host.key('Return');record('native-message-receipt-and-navigation')
        control(dict(type='create_window',session_id=session['id'],title='secondary',spawn=dict(program='/bin/bash',argv=['--noprofile','--norc']),cols=80,rows=24))
        second=next(s for s in snapshot()['sessions'] if s['id']==session['id'])['windows'][-1]['panes'][0]
        wait(lambda:next(p for s in snapshot()['sessions'] for w in s['windows'] for p in w['panes'] if p['id']==second['id']).get('child_pid'),'secondary PTY')
        time.sleep(1)
        cli('pane-write',second['id'],'--text','printf EXACT_SECONDARY','--submit','enter','--json')
        host.key('ctrl+alt+m');ux.run('xdotool','type','--clearmodifiers',f"pane {second['id']}")
        def exact_focus():
            status=host.status()
            return any(p['pane_id']==f"PaneId({status['focused_pane']})" and p.get('remote_pane_id')==second['id'] for p in status['current_panes'])
        if atspi_address:
            nodes=atspi.tree(atspi_address)
            node=next(n for n in nodes if f"/ pane {second['id']} " in n['name'])
            result=atspi.run('--address',atspi_address,'--dest',node['bus'],'--object-path',node['path'],'--method','org.a11y.atspi.Action.DoAction','0')
            assert 'true' in result,result
            (out/'atspi-messages.json').write_text(json.dumps(nodes,indent=2))
        else:host.key('Return')
        try:wait(exact_focus,'exact secondary pane navigation')
        except AssertionError:
            (out/'failed-exact-status.json').write_text(json.dumps(host.status(),indent=2))
            host.capture('failed-exact');raise
        host.capture('exact-secondary-pane');record('exact-secondary-pane-navigation',native_action=bool(atspi_address))
        result=json.loads(cli('restart','--host'))
        assert result['components'][0]['response']['status']=='deferred',result
        record('restart-defers-unrestorable-secondary-view')
        # Close only the extra host view; the daemon PTY keeps running.
        host.key('ctrl+alt+w');wait(lambda:host.status()['tab_count']==1,'close extra view')
        host.key('ctrl+alt+b');wait(lambda:host.status().get('local_terminal_panes'),'blank pane')
        result=json.loads(cli('restart','--host'));assert result['components'][0]['response']['status']=='deferred',result
        record('host-restart-defers-owned-blank-terminal',receipt=result)
        mcp_log=(out/'mcp.log').open('w')
        mcp=subprocess.Popen(['pmux-mcp','--as','polish-mcp','--supervise'],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=mcp_log,text=True,bufsize=1,start_new_session=True)
        reply=rpc(dict(jsonrpc='2.0',id=1,method='initialize',params=dict(protocolVersion='2025-06-18',capabilities={},clientInfo=dict(name='polish',version='1'))));assert 'result' in reply
        rpc(dict(jsonrpc='2.0',method='notifications/initialized'))
        wait(lambda:json.loads(cli('restart','--mcp','--plan'))['mcp_supervisor_pids'],'MCP registration')
        old=children(mcp.pid);result=json.loads(cli('restart','--mcp'))
        assert any(c.get('response',{}).get('status')=='restarted' for c in result['components']),result
        assert children(mcp.pid)!=old
        reply=rpc(dict(jsonrpc='2.0',id=2,method='tools/list',params={}));assert reply['result']['tools']
        record('mcp-cooperative-restart-keeps-protocol',receipt=result)
        (out/'versions.json').write_text(cli('versions')+'\n')
        # Exercise the results UI in the replacement process too.
        host.key('ctrl+alt+u')
        if atspi_address:
            nodes=wait(lambda:(lambda rows:rows if any('Installed and running versions' in n['name'] for n in rows) else None)(atspi.tree(atspi_address)),'replacement maintenance menu')
            node=next(n for n in nodes if 'Installed and running versions' in n['name'])
            atspi.run('--address',atspi_address,'--dest',node['bus'],'--object-path',node['path'],'--method','org.a11y.atspi.Action.DoAction','0')
            wait(lambda:any('Installed pmux:' in n['name'] for n in atspi.tree(atspi_address)),'replacement versions result',seconds=15)
        host.key('Escape')

        # Run the requested sustained workload only after the other checks.
        if a.soak_seconds:
            metrics=[];deadline=time.monotonic()+a.soak_seconds;iteration=0
            blank=host.status()['local_terminal_panes']
            cpu_start={name:sum(map(int,Path(f'/proc/{pid}/stat').read_text().split()[13:15])) for name,pid in [('host',host.process.pid),('daemon',daemon.pid)]}
            while time.monotonic()<deadline:
                start=time.monotonic();cli('send',pane['id'],'seq 1 2000','--enter','--literal','--force')
                width=900 if iteration%2 else 1200
                ux.run('xdotool','windowsize',host.wid,str(width),'700')
                target='polish-b' if iteration%2==0 else 'polish-a'
                cli('space','open',target,'--no-attach')
                wait(lambda:host.status()['space']==target and not host.status()['space_open_pending'],'Space switch under load',seconds=5)
                status=host.status()
                assert status['local_terminal_panes']==(blank if target=='polish-a' else []),status
                rss={name:int(next(line.split()[1] for line in Path(f'/proc/{pid}/status').read_text().splitlines() if line.startswith('VmRSS:'))) for name,pid in [('host',host.process.pid),('daemon',daemon.pid)]}
                metrics.append(dict(iteration=iteration,latency_ms=(time.monotonic()-start)*1000,rss_kib=rss,space=status['space'],cpu_ticks={name:sum(map(int,Path(f'/proc/{pid}/stat').read_text().split()[13:15]))-cpu_start[name] for name,pid in [('host',host.process.pid),('daemon',daemon.pid)]}))
                iteration+=1;time.sleep(.2)
            (out/'soak.json').write_text(json.dumps(metrics,indent=2))
            assert max(m['latency_ms'] for m in metrics)<3000,metrics
            for name in ('host','daemon'):
                assert max(m['rss_kib'][name] for m in metrics)<1024*1024, f'{name} exceeded 1 GiB RSS'
            record('bounded-output-resize-space-soak',iterations=iteration,seconds=a.soak_seconds,max_latency_ms=max(m['latency_ms'] for m in metrics))
        (out/'result.json').write_text(json.dumps(dict(status='PASS',records=records),indent=2)+'\n')
    finally:
        if mcp is not None:
            mcp.stdin.close()
            try:mcp.wait(timeout=5)
            except subprocess.TimeoutExpired:os.killpg(mcp.pid,signal.SIGTERM);mcp.wait(timeout=5)
        if host is not None and host.process.poll() is None:
            try:
                host.key('Escape');host.key('Escape')
                for _ in range(8):
                    if host.process.poll() is not None:break
                    host.key('ctrl+shift+x');time.sleep(.2)
                host.process.wait(timeout=3)
            except (subprocess.TimeoutExpired,subprocess.CalledProcessError):host.stop()
        if daemon is not None and daemon.poll() is None:daemon.terminate();daemon.wait(timeout=5)
        xvfb.terminate();xvfb.wait(timeout=5)
        registry.terminate();registry.wait(timeout=5)
        a11y_bus.terminate();a11y_bus.wait(timeout=5);a11y_log.close()
        log.close();display_log.close()
