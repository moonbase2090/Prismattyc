#!/usr/bin/env python3
"""Termwright RPC resize proof (0.2.0 YAML steps do not support resize)."""
import argparse,base64,json,subprocess
from pathlib import Path
p=argparse.ArgumentParser(description=__doc__)
p.add_argument('--termwright',default='termwright');p.add_argument('--binary',required=True);p.add_argument('--out',type=Path,required=True)
a=p.parse_args();a.out.mkdir(parents=True,exist_ok=True)
sock=subprocess.check_output([a.termwright,'daemon','--background','--cols','80','--rows','24','--',a.binary,'/bin/sh'],text=True).strip()
trace=[]
def rpc(method,params=None):
    result=json.loads(subprocess.check_output([a.termwright,'exec','--socket',sock,'--method',method,'--params',json.dumps(params or {})],text=True,timeout=20))
    assert not result.get('error') or (method=='close' and result['error'].get('code')=='closing'),result
    trace.append(dict(method=method,params=params,result=result if method!='screenshot' else 'PNG saved'))
    return result.get('result')
try:
    rpc('wait_for_idle',dict(idle_ms=400,timeout_ms=15000))
    rpc('type',dict(text="printf 'REFLOW_%s_END\\n' abcdefghijklmnopqrstuvwxyz"));rpc('press',dict(key='Enter'))
    rpc('wait_for_text',dict(text='REFLOW_abcdefghijklmnopqrstuvwxyz_END',timeout_ms=10000))
    for columns,name in [(24,'reflow-narrow'),(80,'reflow-wide')]:
        rpc('resize',dict(cols=columns,rows=24))
        rpc('wait_for_idle',dict(idle_ms=500,timeout_ms=10000))
        if columns==80:rpc('wait_for_text',dict(text='REFLOW_abcdefghijklmnopqrstuvwxyz_END',timeout_ms=10000))
        (a.out/(name+'.png')).write_bytes(base64.b64decode(rpc('screenshot')['png_base64']))
        (a.out/(name+'.json')).write_text(json.dumps(rpc('screen',dict(format='text')),indent=2))
    print('PASS classic-reflow: narrow/wide logical line preserved')
finally:
    rpc('close');(a.out/'trace.json').write_text(json.dumps(trace,indent=2))
