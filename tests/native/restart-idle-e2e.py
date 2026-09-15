#!/usr/bin/env python3
"""Check a safe daemon restart in a disposable socket and installation scope."""
import argparse,json,os,socket,subprocess,tempfile,time
from pathlib import Path
p=argparse.ArgumentParser(description=__doc__);p.add_argument('--bins',type=Path,required=True);p.add_argument('--out',type=Path,required=True);a=p.parse_args();bins=a.bins.resolve();a.out.mkdir(parents=True,exist_ok=False)
records=[]
with tempfile.TemporaryDirectory(prefix='polish-idle-') as tmp:
    root=Path(tmp);sock=root/'pmux.sock'
    env={k:v for k,v in os.environ.items() if not k.startswith(('PMUX_','PRISMATTYC_'))}
    env.update(HOME=tmp,XDG_DATA_HOME=str(root/'data'),XDG_CONFIG_HOME=str(root/'config'),XDG_STATE_HOME=str(root/'state'),XDG_RUNTIME_DIR=tmp,PMUX_SOCKET=str(sock),PMUX_SERVER=str(bins/'pmuxd'),PATH=str(bins)+os.pathsep+env['PATH'])
    def cli(*args,success=True):
        r=subprocess.run([str(bins/'pmux'),*args],env=env,capture_output=True,text=True,timeout=30)
        records.append(dict(args=args,status=r.returncode,stdout=r.stdout,stderr=r.stderr))
        assert (r.returncode==0)==success,records[-1]
        return json.loads(r.stdout) if r.stdout.strip().startswith('{') else r.stdout
    def control(request):
        with socket.socket(socket.AF_UNIX) as conn:
            conn.settimeout(5);conn.connect(str(sock));conn.sendall((json.dumps(dict(version=1,request_id=1,**request))+'\n').encode())
            result=json.loads(conn.makefile().readline());assert 'response' in result,result;return result['response']
    try:
        cli('up');before=cli('versions')['daemon']['pid']
        cli('restart','--all','--plan','--json')
        cli('restart','--host','--stop-sessions',success=False)
        cli('restart','--invalid',success=False)
        updates=root/'data/prismattyc/updates';updates.mkdir(parents=True)
        (updates/'current').symlink_to(bins,target_is_directory=True)
        for session in control(dict(type='snapshot'))['snapshot']['sessions']:
            control(dict(type='destroy_session',session_id=session['id']))
        result=cli('restart','--daemon','--json')
        assert result['components'][0]['status']=='restarted',result
        after=cli('versions')['daemon']['pid'];assert after!=before
        cli('restart','--all','--json')
        cli('restart','--help')
        scheduled=cli('restart','--daemon','--stop-sessions','--json')
        assert scheduled['status']=='scheduled',scheduled
        deadline=time.monotonic()+15
        while time.monotonic()<deadline:
            result=cli('versions')
            if result['daemon'].get('pid') not in (None,after):break
            time.sleep(.1)
        else:raise AssertionError('detached daemon restart did not complete')
        print('PASS idle daemon restart, safe all, plan, invalid flags',flush=True)
    finally:
        cli('stop')
(a.out/'result.json').write_text(json.dumps(dict(status='PASS',records=records),indent=2))
