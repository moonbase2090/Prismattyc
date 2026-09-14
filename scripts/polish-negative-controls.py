#!/usr/bin/env python3
"""Run bounded no-op controls in a disposable source copy, never the live tree."""
import argparse
import json
import os
from pathlib import Path
import re
import shutil
import subprocess

root=Path(__file__).resolve().parents[1]
parser=argparse.ArgumentParser(description=__doc__)
parser.add_argument('--out',type=Path,default=root/'build/polish/negative-controls')
out=parser.parse_args().out.resolve();out.mkdir(parents=True,exist_ok=False)
source=out/'source';source.mkdir()
files=subprocess.check_output(['git','ls-files','-co','--exclude-standard'],cwd=root,text=True).splitlines()
for name in files:
    if '__pycache__' in name or name.startswith('build/'):continue
    original=root/name
    if not original.is_file():continue
    target=source/name;target.parent.mkdir(parents=True,exist_ok=True);shutil.copy2(original,target)
env=dict(os.environ,CARGO_TARGET_DIR=str(root/'target'))
env.pop('PRISMATTYC_HOST',None)
main='crates/prismattyc-host/src/main.rs'
controls=[(main,'pump'),(main,'open_space_from_host'),(main,'poll_host_attach_tabs'),(main,'persist_attach_selection'),(main,'save_space_from_host'),('crates/prismattyc-host/src/render_diagnostics.rs','publish_render_status'),('crates/prismattyc-host/src/space_open.rs','drop')]
results=[]
for name,function in controls:
    path=source/name;original=path.read_text()
    # Find the body by balanced delimiters. These signatures have no braces.
    match=re.search(r'\bfn '+function+r'\s*\(',original);assert match,function
    start=original.index('{',match.end());depth=1;end=start+1
    # Use a lexical scan so braces in strings and comments do not terminate it.
    state='code';quote='';i=end
    while depth:
        c=original[i];n=original[i:i+2]
        if state=='line':
            if c=='\n':state='code'
        elif state=='string':
            if c=='\\':i+=1
            elif c=='"':state='code'
        elif state=='code':
            if n=='//':state='line';i+=1
            elif c=='"':state='string'
            elif c=='{':depth+=1
            elif c=='}':depth-=1
        i+=1
    changed=original[:start]+'{ }'+original[i:]
    path.write_text(changed)
    test='dropping_open_kills_reaps_and_prevents_late_cache_writes' if function=='drop' else 'isolated_space_windows_create_move_and_render'
    try:
        with (out/(function+'.log')).open('w') as log:
            result=subprocess.run(['cargo','test','-p','prismattyc-host','--bin','prismattyc-host','--locked',test,'--','--nocapture','--test-threads=1'],cwd=source,env=env,stdout=log,stderr=subprocess.STDOUT,timeout=180)
        text=(out/(function+'.log')).read_text()
        caught=result.returncode!=0 and ('test result: FAILED' in text or 'window test child timed out' in text) and 'could not compile' not in text
        results.append(dict(file=name,function=function,caught=caught,exit=result.returncode))
        (out/'result.json').write_text(json.dumps(dict(scope='seven explicit no-op controls, not a full mutation gate',results=results),indent=2))
        print(('CAUGHT ' if caught else 'NOT CAUGHT ')+function,flush=True)
    finally:path.write_text(original)
assert len(results)==7 and all(r['caught'] for r in results),results
