#!/usr/bin/env python3
"""Exercise the updater CLI with local curl fixtures and disposable installations."""
import argparse,hashlib,json,os,platform
from pathlib import Path
import subprocess,tempfile,time
p=argparse.ArgumentParser(description=__doc__);p.add_argument('--pmux',type=Path,required=True);p.add_argument('--out',type=Path,required=True);a=p.parse_args();binary=a.pmux.resolve();out=a.out.resolve();out.mkdir(parents=True,exist_ok=False)
target = {'x86_64': 'x86_64-unknown-linux-gnu', 'aarch64': 'aarch64-unknown-linux-gnu'}[platform.machine()]
records=[]
installed_label=subprocess.check_output([str(binary),'--version'],text=True).split()
version=next(word for word in installed_label if len(word.split('.'))==3 and all(part.isdecimal() for part in word.split('.')))
major,minor,patch=map(int,version.split('.'))
initial=f'{major}.{minor}.{patch+1}'
following=f'{major}.{minor}.{patch+2}'
with tempfile.TemporaryDirectory(prefix='release-update-') as tmp:
    root=Path(tmp);bins=root/'bin';tools=root/'tools';assets=root/'assets'
    for directory in [bins,tools,assets]:directory.mkdir()
    names=('pmux','pmuxd','pmux-attach','pmux-mcp','prismattyc','prismattyc-host')
    for name in names:
        path=bins/name;path.write_text('#!/bin/sh\necho '+name+' 0.1.319\n');path.chmod(0o755)
    shim=tools/'curl';shim.write_text('''#!/usr/bin/env python3
import os,pathlib,sys,time
args=sys.argv[1:];root=pathlib.Path(os.environ['FIXTURE_ROOT'])
if '--output' not in args:
 print((root/'metadata.json').read_text());sys.exit(0)
name=args[-1].split('/')[-1]
if os.environ.get('INTERRUPT') and name.endswith('-pmux-attach'):
 (root/'paused').write_text('yes');time.sleep(30)
pathlib.Path(args[args.index('--output')+1]).write_bytes((root/'assets'/name).read_bytes())
''');shim.chmod(0o755)
    env=dict(os.environ,XDG_DATA_HOME=str(root/'data'),HOME=str(root),PATH=str(tools)+os.pathsep+os.environ['PATH'],FIXTURE_ROOT=str(root))
    def release(version):
        metadata=dict(tag_name='v'+version,draft=False,prerelease=False,immutable=True,assets=[])
        for name in names:
            file=assets/f'prismattyc-v{version}-{target}-{name}';file.write_text(f'#!/bin/sh\necho {name} {version}\n')
            metadata['assets'].append(dict(name=file.name,size=file.stat().st_size,digest='sha256:'+hashlib.sha256(file.read_bytes()).hexdigest(),browser_download_url='https://github.com/moonbase2090/Prismattyc/releases/download/v'+version+'/'+file.name))
        (root/'metadata.json').write_text(json.dumps(metadata));return metadata
    def cli(*args,success=True):
        result=subprocess.run([str(binary),'update',*args],env=env,capture_output=True,text=True,timeout=15)
        assert (result.returncode==0)==success,result.stderr
        records.append(dict(args=args,exit=result.returncode,stdout=result.stdout,stderr=result.stderr));return result
    def installed(version):
        for name in names:assert subprocess.check_output([str(bins/name),'--version'],text=True).strip()==f'{name} {version}'
    metadata=release(initial)
    cli('--check','--json')
    child=subprocess.Popen([str(binary),'update','--bin-dir',str(bins)],env=dict(env,INTERRUPT='1'),stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,start_new_session=True)
    try:
        deadline=time.monotonic()+8
        while not (root/'paused').exists():
            assert child.poll() is None and time.monotonic()<deadline;time.sleep(.02)
    finally:
        import signal
        if child.poll() is None:os.killpg(child.pid,signal.SIGTERM)
        child.wait(timeout=5)
    installed('0.1.319');records.append(dict(check='interrupted-download-preserves-all-old-binaries'))
    # Corrupt one member without changing the trusted fixture digest.
    corrupted=assets/metadata['assets'][2]['name'];original=corrupted.read_bytes();corrupted.write_bytes(original.replace(b'echo',b'exit'))
    cli('--bin-dir',str(bins),success=False);installed('0.1.319');corrupted.write_bytes(original)
    cli('--bin-dir',str(bins));installed(initial)
    assert not list((root/'data/prismattyc/updates').glob('.stage-*'))
    cli('--rollback');installed('0.1.319')
    cli('--bin-dir',str(bins));installed(initial)
    metadata=release(following);metadata['immutable']=False;(root/'metadata.json').write_text(json.dumps(metadata))
    cli('--bin-dir',str(bins),success=False);installed(initial)
    release(following);cli('--json');installed(following)
    cli('--rollback');installed(initial)
(out/'result.json').write_text(json.dumps(dict(status='PASS',transport='local curl fixture; no production release published',records=records),indent=2)+'\n')
print('PASS interruption, digest rejection, install, rollback, retry, immutable channel, next version',flush=True)
