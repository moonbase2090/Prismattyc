#!/usr/bin/env python3
"""Check intentional pane writes against five isolated PTY agent adapters."""
import argparse, json, os, pathlib, socket, subprocess, tempfile, time
parser=argparse.ArgumentParser(description=__doc__)
parser.add_argument('--bins', type=pathlib.Path, required=True)
parser.add_argument('--out', type=pathlib.Path, required=True)
args=parser.parse_args()
out=args.out.resolve();out.mkdir(parents=True,exist_ok=True)
bins=args.bins.resolve()
records=[]
def wait(fn):
    deadline=time.monotonic()+5
    while time.monotonic()<deadline:
        v=fn()
        if v:return v
        time.sleep(.03)
    raise AssertionError('fixture timeout')
with tempfile.TemporaryDirectory(prefix='pane-write-') as tmp:
    tmp=pathlib.Path(tmp); path=str(tmp/'mux.sock')
    env={k:v for k,v in os.environ.items() if not k.startswith(('PMUX_', 'PRISMATTYC_', 'HIVE_'))}
    env['HOME']=str(tmp);env['XDG_DATA_HOME']=str(tmp/'data');env['PATH']=str(bins)+':'+env['PATH']
    log=(out/'daemon.log').open('w')
    daemon=subprocess.Popen([str(bins/'pmuxd'),'--socket',path,'--','/bin/sh'],env=env,stdout=log,stderr=log)
    def cli(*args,body=None):
        result=subprocess.run([str(bins/'pmux'),'--socket',path,*map(str,args)],env=env,input=body,capture_output=True,text=True,timeout=6)
        records.append(dict(args=args,exit=result.returncode,stdout=result.stdout,stderr=result.stderr))
        return result
    def snapshot():
        with socket.socket(socket.AF_UNIX) as s:
            s.connect(path);s.sendall(b'{"version":1,"request_id":1,"type":"snapshot"}\n')
            return json.loads(s.makefile().readline())['response']['snapshot']
    try:
        wait(lambda:os.path.exists(path))
        for agent in ['grok','claude','kiro','codex','cursor-agent']:
            program=tmp/agent; ready=tmp/(agent+'.ready'); captured=tmp/(agent+'.captured')
            body='Review λ\nKeep literal \\n and --help.'
            suffix=b'\x1b[13;5u' if agent=='cursor-agent' else b'\r\r' if agent=='codex' else b'\r'
            expected=b'\x1b[200~'+body.encode()+b'\x1b[201~'+suffix
            program.write_text('#!/usr/bin/python3\nimport os,tty,pathlib,time\ntty.setraw(0)\npathlib.Path('+repr(str(ready))+').touch()\ndata=b""\nwhile len(data)<'+str(len(expected))+':\n data+=os.read(0,65536)\npathlib.Path('+repr(str(captured))+').write_bytes(data)\nprint("AGENT_RECEIVED",flush=True)\ntime.sleep(10)\n')
            program.chmod(0o755)
            result=cli('new','--no-attach',agent,'--',str(program));assert result.returncode==0,result.stderr
            wait(ready.exists)
            snap=snapshot(); pane=next(s for s in snap['sessions'] if s['name']==agent)['windows'][0]['panes'][0]['id']
            result=cli('pane-write',pane,'--stdin','--json',body=body)
            assert result.returncode==0,result.stderr
            receipt=json.loads(result.stdout);assert receipt['status']=='queued',receipt
            assert receipt['response']['nbytes']==len(expected),receipt
            wait(captured.exists)
            assert captured.read_bytes()==expected,(agent,captured.read_bytes(),expected)
            assert not next(s for s in snapshot()['sessions'] if s['name']==agent)['windows'][0]['panes'][0]['ledger']['dirty_input']
            print('PASS '+agent+' multiline paste, literal UTF-8, submit, exact byte receipt',flush=True)
            cli('stop',agent)
        (out/'acceptance-result.json').write_text(json.dumps({'status':'PASS','agent_stubs':5,'records':records},indent=2)+'\n')
    finally:
        daemon.terminate();daemon.wait(timeout=5);log.close()
