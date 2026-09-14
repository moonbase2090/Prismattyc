#!/usr/bin/env python3
"""Record a private two-pane collaboration demo, screenshots, and receipts."""
import argparse, importlib.util, json, os, pathlib, signal, socket, subprocess, tempfile, time


def wait(fn, label):
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        value = fn()
        if value:
            return value
        time.sleep(.1)
    raise RuntimeError('timeout: ' + label)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bins', type=pathlib.Path, required=True)
    parser.add_argument('--out', type=pathlib.Path, required=True)
    args = parser.parse_args()
    out = args.out.resolve(); out.mkdir(parents=True, exist_ok=False)
    bins = args.bins.resolve()
    result = {'status': 'RUNNING', 'receipts': []}
    host = daemon = display = video = None
    with tempfile.TemporaryDirectory(prefix='pmux-demo-') as runtime:
        for key in list(os.environ):
            if key.startswith(('PMUX_', 'PRISMATTYC_', 'HIVE_', 'XDG_')) or key == 'WAYLAND_DISPLAY':
                del os.environ[key]
        home = out / 'home'; home.mkdir()
        os.environ.update(HOME=str(home), PATH=str(bins)+':'+os.environ['PATH'], SHELL='/bin/bash',
            PMUX_SOCKET=runtime+'/mux.sock', XDG_RUNTIME_DIR=runtime, HOST_UX_NO_WM='1',
            HOST_UX_OUT=str(out/'frames'), WINIT_UNIX_BACKEND='x11', PS1='DEMO> ')
        for kind in ['data', 'config', 'state']:
            path=out/kind;path.mkdir();os.environ['XDG_'+kind.upper()+'_HOME']=str(path)
        def cli(*args):
            p=subprocess.run(['pmux',*map(str,args)],capture_output=True,text=True,timeout=20)
            result['receipts'].append({'args':list(map(str,args)),'exit':p.returncode,'stdout':p.stdout,'stderr':p.stderr})
            assert p.returncode==0, p.stderr
            return p.stdout
        def snapshot():
            with socket.socket(socket.AF_UNIX) as s:
                s.connect(os.environ['PMUX_SOCKET']);s.sendall(b'{"version":1,"request_id":1,"type":"snapshot"}\n')
                return json.loads(s.makefile().readline())['response']['snapshot']
        def body(pane): return cli('save-buffer',str(pane),'-')
        def write(pane,text):
            receipt=json.loads(cli('pane-write',pane,'--text',text,'--submit','enter','--json'))
            assert receipt['response']['complete'],receipt
        try:
            daemon=subprocess.Popen(['pmuxd','--socket',os.environ['PMUX_SOCKET'],'--','/bin/bash','--noprofile','--norc'],stdout=(out/'daemon.log').open('w'),stderr=subprocess.STDOUT,cwd=home)
            wait(lambda:pathlib.Path(os.environ['PMUX_SOCKET']).exists(),'daemon')
            cli('space','create','Collaboration','--session-name','sender','--no-attach')
            cli('space','add','Collaboration','--name','receiver')
            view=pathlib.Path(runtime)/'mux.attach-tabs.json'
            cli('space','open','Collaboration','--no-attach','--no-run','--view-path',view)
            s=json.loads(view.read_text());ids=[i for t in s['tabs'] for i in t['sessions']]
            s['tabs']=[{'title':'Direct collaboration','sessions':ids}];s['active_tab']=0;s['focused_session']=ids[0]
            view.write_text(json.dumps(s));os.environ['PMUX_VIEW_PATH']=str(view)
            sessions={s['name']:s for s in snapshot()['sessions']}
            sender=sessions['sender']['windows'][0]['panes'][0]['id'];receiver=sessions['receiver']['windows'][0]['panes'][0]['id']
            display=subprocess.Popen(['Xvfb','-displayfd','1','-screen','0','1920x1080x24','-nolisten','tcp','-noreset'],stdout=subprocess.PIPE,stderr=subprocess.DEVNULL)
            os.environ['DISPLAY']=':'+display.stdout.readline().decode().strip()
            spec=importlib.util.spec_from_file_location('ux',pathlib.Path(__file__).with_name('host-ux-e2e.py'));ux=importlib.util.module_from_spec(spec);spec.loader.exec_module(ux)
            host=ux.Host('pane-messaging',['--attach-session',str(sessions['sender']['id'])], 'font_px = 16.0\nspace_startup = "restore"\nspace_rail = "bottom"\n')
            ux.run('xdotool','windowsize','--sync',host.wid,'1600','900')
            ux.run('xdotool','mousemove','1910','1070')
            wait(lambda:not host.status()['space_open_pending'],'initial Space open')
            view.write_text(json.dumps(s))
            wait(lambda:host.status()['pane_count']==2,'two visible panes')
            video=subprocess.Popen(['ffmpeg','-y','-loglevel','error','-f','x11grab','-video_size','1920x1080','-framerate','15','-i',os.environ['DISPLAY'],'-c:v','libx264','-preset','ultrafast','-pix_fmt','yuv420p',str(out/'pane-messaging.mp4')],stdout=subprocess.DEVNULL,stderr=(out/'video.log').open('w'))
            time.sleep(1)
            write(sender,"printf 'SENDER: commands and receipts\\n'")
            write(receiver,"printf 'RECEIVER: a separate shell pane\\n'")
            time.sleep(1);host.capture('01-ready');host.display('01-ready')
            write(sender,f"pmux pane-write {receiver} --text 'seq 1 10000' --submit enter --json")
            wait(lambda:any(line.strip()=='10000' for line in body(receiver).splitlines()),'seq output')
            wait(lambda:'"complete":true' in body(sender).replace('\n','').replace(' ',''),'sender receipt')
            time.sleep(1);host.capture('02-command');host.display('02-command');time.sleep(3)
            write(sender,'pmux space remove Collaboration --session receiver --kill')
            wait(lambda:all(s['name']!='receiver' for s in snapshot()['sessions']),'session removed')
            wait(lambda:host.status()['pane_count']==1,'pane removed from host')
            time.sleep(1)
            write(sender,"printf 'Receiver removed and killed. Sender is still running.\\n'")
            time.sleep(1);host.capture('03-cleanup');host.display('03-cleanup');time.sleep(3)
            result['status']='PASS'
        except Exception as error:
            result.update(status='FAIL',error=str(error));raise
        finally:
            if video:
                video.send_signal(signal.SIGINT);video.wait(timeout=15)
            if host:host.stop()
            if display:display.terminate();display.wait(timeout=5)
            if daemon:daemon.terminate();daemon.wait(timeout=5)
            (out/'result.json').write_text(json.dumps(result,indent=2)+'\n')
    (out/'index.html').write_text('''<!doctype html><meta charset="utf-8"><title>Prismattyc pane messaging</title>
<style>body{background:#17191f;color:#e7e9f0;font:18px system-ui;max-width:1200px;margin:40px auto;padding:20px}img,video{width:100%;border:1px solid #555}p{line-height:1.6}code{color:#aaaaff}</style>
<h1>Intentional pane messaging</h1><p>A real sender shell writes to a receiver in the same Space. The demo checks the output, then removes and kills only the receiver.</p>
<video controls src="pane-messaging.mp4"></video>
<h2>1. Two panes in one Space</h2><p>Sender and receiver are independent processes.</p><img src="frames/pane-messaging/01-ready.png">
<h2>2. Run a command through pane messaging</h2><p><code>pmux pane-write PANE --text 'seq 1 10000' --submit enter --json</code> queues the command. The receiver reaches 10000. A queue receipt and observed output are separate evidence.</p><img src="frames/pane-messaging/02-command.png">
<h2>3. Remove and kill the receiver</h2><p>The receiver session and pane are gone. The sender remains running.</p><img src="frames/pane-messaging/03-cleanup.png">
<p>This demo uses a private daemon and display. It does not use AWS credentials or call an AI provider.</p>''')
    print(out/'index.html')


if __name__=='__main__':main()
