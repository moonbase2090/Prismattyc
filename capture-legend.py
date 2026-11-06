#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Capture the real native legend on X11 at the reviewed revision."""
import hashlib,json,os,signal,struct,subprocess,time
from pathlib import Path

ROOT=Path(os.environ.get('LEGEND_PROOF_ROOT','.')).resolve()
BINS=ROOT/'target/debug'
OUT=ROOT/'captures'
OUT.mkdir(exist_ok=True)
SHA='47754266530581ba8a69debda94c28dbf0c186e1'

def run(*args,env=None):
 return subprocess.check_output(args,text=True,env=env,stderr=subprocess.PIPE).strip()
def wait(fn,label,seconds=25):
 deadline=time.monotonic()+seconds
 while time.monotonic()<deadline:
  try:
   value=fn()
   if value:return value
  except (OSError,ValueError,KeyError,subprocess.CalledProcessError):pass
  time.sleep(.1)
 raise AssertionError('Timed out: '+label)
def stop(p):
 if p and p.poll() is None:
  p.terminate()
  try:p.wait(timeout=5)
  except subprocess.TimeoutExpired:p.kill();p.wait(timeout=5)
def png_size(path):
 data=path.read_bytes()
 assert data[:8]==b'\x89PNG\r\n\x1a\n'
 return struct.unpack('>II',data[16:24])

env=dict(os.environ,DISPLAY=':99',WINIT_UNIX_BACKEND='x11')
env.pop('WAYLAND_DISPLAY',None)
display=wm=None
results=[]
try:
 display=subprocess.Popen(['Xvfb',':99','-screen','0','3200x1800x24','-ac','-noreset'],stdout=(ROOT/'xvfb.log').open('w'),stderr=subprocess.STDOUT)
 wait(lambda:run('xdpyinfo',env=env),'X11 desktop')
 wm=subprocess.Popen(['openbox'],env=env,stdout=(ROOT/'openbox.log').open('w'),stderr=subprocess.STDOUT)
 time.sleep(1)
 for theme in ['prismattyc-default','prismattyc-light']:
  for scale in [1,2]:
   name=('dark' if theme.endswith('default') else 'light')+'-'+str(scale)+'x'
   case=ROOT/name;case.mkdir(exist_ok=True)
   runtime=Path('/tmp')/('legend-'+name);runtime.mkdir(exist_ok=True);runtime.chmod(0o700)
   config=case/'config.toml'
   config.write_text('theme = "'+theme+'"\nchrome_style = "graphite"\nfont_px = 16.0\nsplash = false\nstart_at_login = false\nautomatic_update_checks = false\ninstall_agent_skills = false\nspace_rail = "bottom"\nwindow_opacity = 1.0\nwindow_blur = false\n')
   present=case/'present.png'
   case_env=dict(env,PATH=str(BINS)+':/usr/bin:/bin',PMUX_SOCKET=str(runtime/'pmux.sock'),XDG_RUNTIME_DIR=str(runtime),XDG_CONFIG_HOME=str(case/'config'),XDG_STATE_HOME=str(case/'state'),XDG_DATA_HOME=str(case/'data'),PRISMATTYC_CONFIG=str(config),PRISMATTYC_NO_AGENT_SKILLS='1',WINIT_X11_SCALE_FACTOR=str(scale),PRISMATTYC_DUMP_PRESENT=str(present),SHELL='/bin/sh',PS1='NATIVE> ')
   daemon=host=None
   try:
    daemon=subprocess.Popen([str(BINS/'pmuxd'),'--socket',case_env['PMUX_SOCKET'],'--','/bin/sh','-c','printf "\033[2J\033[HNative legend validation\n"; sleep 3600'],env=case_env,stdout=(case/'daemon.log').open('w'),stderr=subprocess.STDOUT)
    wait(lambda:Path(case_env['PMUX_SOCKET']).exists(),'private daemon')
    host=subprocess.Popen([str(BINS/'prismattyc-host'),'--no-splash','--attach-session','1'],env=case_env,stdout=(case/'host.log').open('w'),stderr=subprocess.STDOUT)
    wid=wait(lambda:run('xdotool','search','--onlyvisible','--pid',str(host.pid),env=env),'native window').splitlines()[-1]
    run('xdotool','windowmove',wid,'40','40',env=env)
    run('xdotool','windowactivate','--sync',wid,env=env)
    run('xdotool','windowfocus','--sync',wid,env=env)
    run('xdotool','mousemove','3190','1790',env=env)
    def status():
     data=json.loads(run(str(BINS/'pmux'),'render-status','--json',env=case_env))
     assert data['host_pid']==host.pid
     return data['windows'][0]
    def footer():return 'footer-visible' in status()['last_raster']['guards']
    for mode,width,height in [('wide',1280*scale,700*scale),('narrow',530*scale,420*scale)]:
     run('xdotool','windowsize','--sync',wid,str(width),str(height),env=env)
     wait(lambda:present.exists() and png_size(present)==(width,height),'resized present')
     run('xdotool','keydown','Control_L','Shift_L',env=env)
     wait(footer,'held Ctrl+Shift legend')
     time.sleep(2.1)
     assert footer(),'legend vanished while chord held'
     geometry=dict(line.split('=',1) for line in run('xdotool','getwindowgeometry','--shell',wid,env=env).splitlines())
     assert int(geometry['WIDTH'])==width and int(geometry['HEIGHT'])==height,geometry
     shot=OUT/(name+'-'+mode+'.png')
     run('ffmpeg','-hide_banner','-loglevel','error','-f','x11grab','-window_id',wid,'-video_size',str(width)+'x'+str(height),'-i',':99','-frames:v','1','-y',str(shot),env=env)
     assert png_size(shot)==(width,height)
     actual=subprocess.check_output(['ffmpeg','-hide_banner','-loglevel','error','-i',str(shot),'-f','rawvideo','-pix_fmt','rgb24','-'])
     presented=subprocess.check_output(['ffmpeg','-hide_banner','-loglevel','error','-i',str(present),'-f','rawvideo','-pix_fmt','rgb24','-'])
     assert actual==presented,'native window capture differs from real present pixels'
     assert status()['tab_strip_height']==44*scale, status()['tab_strip_height']
     item=dict(tab_strip_height=status()['tab_strip_height'],revision=SHA,theme=theme,scale_factor=scale,mode=mode,window_px=[width,height],footer_visible=True,screenshot=shot.name,sha256=hashlib.sha256(shot.read_bytes()).hexdigest())
     (OUT/(name+'-'+mode+'-status.json')).write_text(json.dumps(status(),indent=2))
     results.append(item)
     run('xdotool','keyup','Shift_L','Control_L',env=env)
     wait(lambda:not footer(),'legend clears after release',seconds=10)
     print('PASS',name,mode,width,height,flush=True)
   finally:
    run('xdotool','keyup','Shift_L','Control_L',env=env)
    stop(host);stop(daemon)
 (OUT/'result.json').write_text(json.dumps(dict(revision=SHA,status='PASS',backend='X11 native winit window, softbuffer, Xvfb and Openbox',cases=results),indent=2))
finally:
 stop(wm);stop(display)
