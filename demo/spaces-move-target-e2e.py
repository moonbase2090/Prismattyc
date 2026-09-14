#!/usr/bin/env python3
"""A nested session move must preserve every surrounding pane and process."""
import os,json,subprocess,tempfile,time,importlib.util,hashlib,shutil,sys
from pathlib import Path
sys.dont_write_bytecode=True

def wait(fn,label,timeout=20):
 end=time.monotonic()+timeout
 while time.monotonic()<end:
  value=fn()
  if value:return value
  time.sleep(.1)
 raise AssertionError('timeout: '+label)

def main():
 out=Path(os.environ['MOVE_TARGET_OUT']).resolve();out.mkdir(parents=True,exist_ok=False)
 result={'status':'RUNNING','checks':[],'sha256':{n:hashlib.sha256(Path(shutil.which(n)).read_bytes()).hexdigest() for n in ['pmux','pmuxd','pmux-attach','prismattyc-host']}}
 host=daemon=x=None
 with tempfile.TemporaryDirectory(prefix='pmux-move-') as runtime:
  for key in list(os.environ):
   if key.startswith(('PMUX','PRISMATTYC_','XDG_')) or key=='WAYLAND_DISPLAY':del os.environ[key]
  home=out/'home';home.mkdir();os.environ.update(HOME=str(home),SHELL='/bin/bash',PMUX_SOCKET=runtime+'/pmux.sock',XDG_RUNTIME_DIR=runtime,HOST_UX_NO_WM='1',HOST_UX_OUT=str(out/'native'),WINIT_UNIX_BACKEND='x11')
  for kind in ['data','config','state']:
   path=out/kind;path.mkdir();os.environ['XDG_'+kind.upper()+'_HOME']=str(path)
  def cli(*args):
   p=subprocess.run(['pmux',*map(str,args)],capture_output=True,text=True,timeout=25)
   with (out/'commands.log').open('a') as f:f.write(json.dumps({'args':args,'exit':p.returncode,'out':p.stdout,'err':p.stderr})+'\n')
   assert p.returncode==0,(args,p.stderr)
   return p.stdout
  def snapshot():
   import socket
   with socket.socket(socket.AF_UNIX,socket.SOCK_STREAM) as s:
    s.connect(os.environ['PMUX_SOCKET']);s.sendall(b'{"type":"snapshot","version":1,"request_id":1}\n')
    return json.loads(s.makefile().readline())['response']['snapshot']['sessions']
  def seats():
   return {p['id']:(s['id'],s['name'],s.get('space_id'),p.get('child_pid')) for s in snapshot() for w in s['windows'] for p in w['panes']}
  def by_name(name):return next((p for p,row in seats().items() if row[1]==name),None)
  def passed(name):result['checks'].append({'name':name,'status':'PASS'});print('PASS '+name,flush=True)
  try:
   daemon=subprocess.Popen(['pmuxd','--socket',os.environ['PMUX_SOCKET'],'--','/bin/bash','--noprofile','--norc'],stdout=(out/'daemon.log').open('w'),stderr=subprocess.STDOUT)
   wait(lambda:Path(os.environ['PMUX_SOCKET']).exists(),'daemon')
   cli('space','create','beta','--session-name','beta-1','--no-attach')
   cli('space','create','alpha','--session-name','outer','--no-attach')
   cli('space','add','alpha','--name','sibling')
   cli('space','open','alpha','--no-attach','--no-run')
   alpha=json.loads((out/'data/prismattyc/spaces/alpha.json').read_text())['id'];beta=json.loads((out/'data/prismattyc/spaces/beta.json').read_text())['id']
   spec=importlib.util.spec_from_file_location('ux',Path(__file__).with_name('host-ux-e2e.py'));ux=importlib.util.module_from_spec(spec);spec.loader.exec_module(ux)
   x=subprocess.Popen(['Xvfb','-displayfd','1','-screen','0','1920x1080x24','-nolisten','tcp','-noreset'],stdout=subprocess.PIPE,stderr=subprocess.DEVNULL);os.environ['DISPLAY']=':'+x.stdout.readline().decode().strip()
   host=ux.Host('move',['--attach-session',str(seats()[by_name('outer')][0])],'font_px = 16.0\nspace_startup = "restore"\nspace_rail = "bottom"\n')
   chip=wait(lambda:next((c for c in host.status()['space_chips'] if c['name']=='alpha'),None),'alpha chip')
   ux.run('xdotool','mousemove','--window',host.wid,str(chip['x']+20),str(chip['y']+15));ux.run('xdotool','click','1')
   wait(lambda:host.status()['space']=='alpha' and not host.status()['space_open_pending'],'source Space');time.sleep(1)
   def shell(command):ux.run('xdotool','type','--clearmodifiers','--delay','8',command);time.sleep(.2);host.key('Return')
   def capture(name):time.sleep(.5);host.capture(name)
   def open_move(index=4):
    slot=host.status()['visible_pane_slots'][0][1]
    ux.run('xdotool','mousemove','--window',host.wid,str(slot[0]+25),str(slot[1]+70));ux.run('xdotool','click','3')
    wait(lambda:host.status()['context_menu'],'pane menu');host.key('Home')
    for _ in range(index):host.key('Down')
    host.key('Return');wait(lambda:host.status()['space_picker_open'],'move picker')
   def choose_beta():ux.run('xdotool','type','--clearmodifiers','beta');host.key('Return');time.sleep(1)
   outer=by_name('outer');before=seats();shell('pmux new claude')
   inner=wait(lambda:by_name('claude'),'nested claude');time.sleep(1)
   inner_before=seats()[inner]
   viewer=int(json.loads(cli('clients','claude','--json'))[0]['pid'])
   shell('pmux whoami');capture('nested-claude-before-move')
   open_move();capture('move-picker-target');choose_beta()
   wait(lambda:seats()[inner][2]==beta,'visible nested pane moved')
   assert {p:seats()[p] for p in before}==before,'an outer or unrelated pane changed'
   assert seats()[inner][0]==inner_before[0] and seats()[inner][3]==inner_before[3]
   wait(lambda:not Path('/proc/'+str(viewer)).exists(),'nested viewer detached cleanly')
   token=out/'parent-token';shell('printf parent-ok > '+str(token));wait(token.exists,'parent shell input');assert token.read_text()=='parent-ok'
   capture('parent-shell-preserved');passed('nested-move-preserves-parent-sibling-and-every-process')
   # A target that exits while the picker is open must never fall back to the parent.
   shell('pmux new transient');transient=wait(lambda:by_name('transient'),'transient session');time.sleep(1)
   open_move();prior=seats();cli('stop','transient');choose_beta()
   assert {p:seats()[p] for p in before}==before
   assert transient not in seats();capture('stale-target-cancelled');passed('stale-picker-target-cancels-without-moving-parent')
   # Direct numeric-name collision: the selected outer session's numeric ID
   # is also a different session's display name. No nested cycle is needed.
   collision_name=str(seats()[outer][0])
   cli('new','--no-attach',collision_name);collision=by_name(collision_name)
   time.sleep(1);before_collision=seats()
   open_move(9);capture('numeric-collision-picker');choose_beta()
   wait(lambda:seats()[outer][2]==beta,'exact focused session moved')
   assert seats()[collision]==before_collision[collision],'numeric-name session moved instead'
   assert seats()[outer][0]==before_collision[outer][0] and seats()[outer][3]==before_collision[outer][3]
   assert {p:seats()[p] for p in before_collision if p!=outer}=={p:row for p,row in before_collision.items() if p!=outer}
   capture('numeric-collision-result');passed('whole-session-context-move-ignores-colliding-numeric-name')
   result['status']='PASS'
  except Exception as error:
   import traceback
   result.update(status='FAIL',error=str(error),traceback=traceback.format_exc())
   (out/'failure-seats.json').write_text(json.dumps(seats(),indent=2))
   if host:
    try:host.capture('failure');(out/'failure-status.json').write_text(json.dumps(host.status(),indent=2))
    except Exception:pass
   raise
  finally:
   if host:host.stop()
   if x:x.terminate();x.wait(timeout=5)
   if daemon:daemon.terminate();daemon.wait(timeout=5)
   (out/'result.json').write_text(json.dumps(result,indent=2)+'\n')
if __name__=='__main__':main()
