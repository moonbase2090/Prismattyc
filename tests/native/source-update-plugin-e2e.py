#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Check source-update orchestration with real bundle verification and isolated Git/Cargo shims.

The shims record build/install requests without touching an installation or remote.
This checks orchestration, not a Cargo installation or native application launch.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

REPO = Path(__file__).resolve().parents[2]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--pmux', type=Path, required=True)
    parser.add_argument('--verifier', type=Path, required=True)
    parser.add_argument('--bundle', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    args = parser.parse_args()
    args.out.mkdir(parents=True, exist_ok=False)
    results = {'status': 'FAIL', 'scope': 'source-update orchestration; actual bundle verifier; Git/Cargo shims'}
    records = []
    try:
        with tempfile.TemporaryDirectory(prefix='source-plugin-update-') as temporary:
            root = Path(temporary).resolve()
            repo = root / 'repo'
            for relative in ['crates/prismattyc-host', 'crates/prismattyc-mux', 'scripts/release', 'tools', 'home']:
                (repo / relative).mkdir(parents=True)
            scripts = repo / 'scripts/release'
            for name in ['plugin_bundle_provenance.py', 'prepare-plugin-bundle.py']:
                shutil.copy2(REPO / 'scripts/release' / name, scripts / name)
            examples = repo / 'target/release/examples'
            examples.mkdir(parents=True)
            shutil.copy2(args.verifier, examples / ('plugin_bundle.exe' if os.name == 'nt' else 'plugin_bundle'))
            tools = repo / 'tools'
            log = root / 'calls.jsonl'
            for name in ['git', 'cargo']:
                path = tools / name
                path.write_text('''#!/usr/bin/env python3
import json,os,sys
from pathlib import Path
name=Path(sys.argv[0]).name
with open(os.environ['SOURCE_PROOF_CALLS'],'a') as output:
 output.write(json.dumps({'program':name,'args':sys.argv[1:],'bundle':os.environ.get('PRISMATTYC_PLUGIN_BUNDLE_DIR'),'flags':os.environ.get('CARGO_ENCODED_RUSTFLAGS')})+'\\n')
if name=='git' and sys.argv[1:2]==['rev-parse']:print('1111111111111111111111111111111111111111')
''')
                path.chmod(0o755)
            env = {k: v for k, v in os.environ.items() if not k.startswith(('PMUX', 'PRISMATTYC_', 'XDG_', 'CARGO_', 'RUSTFLAGS'))}
            env.update(HOME=str(repo / 'home'), PATH=str(tools) + os.pathsep + os.environ['PATH'],
                PRISMATTYC_REPO=str(repo), PRISMATTYC_PLUGIN_BUNDLE_DIR=str(args.bundle.resolve()),
                SOURCE_PROOF_CALLS=str(log), CARGO_TARGET_DIR=str(repo / 'target'))
            proc = subprocess.run([str(args.pmux.resolve()), 'update', '--source', '--mux'], env=env, capture_output=True, text=True, timeout=60)
            calls = [json.loads(line) for line in log.read_text().splitlines()]
            installs = [call for call in calls if call['program'] == 'cargo' and call['args'][:1] == ['install']]
            records.append({'case': 'verified bundle', 'exit': proc.returncode, 'stderr': proc.stderr, 'calls': calls})
            assert proc.returncode == 0, proc.stderr
            paths = [call['args'][call['args'].index('--path') + 1] for call in installs]
            assert paths == ['crates/prismattyc-mux', 'crates/pmux-mcp', 'tools/activity-monitor'], paths
            prepared = Path(installs[0]['bundle'])
            assert prepared != args.bundle.resolve() and prepared.is_relative_to(repo / 'target/plugin-bundles')
            assert (prepared / 'bundle-pin.json').read_bytes() == (args.bundle / 'bundle-pin.json').read_bytes()
            assert all(call['bundle'] == str(prepared) for call in installs)
            assert all(call['flags'] == installs[0]['flags'] for call in installs)
            assert '--remap-path-prefix' in installs[0]['flags'].split('\x1f')
            assert any(flag.endswith('=/workspace') for flag in installs[0]['flags'].split('\x1f'))
            corrupt = root / 'corrupt'
            shutil.copytree(args.bundle, corrupt)
            next(corrupt.glob('*.pmux-plugin')).write_bytes(b'corrupt')
            log.unlink()
            proc = subprocess.run([str(args.pmux.resolve()), 'update', '--source', '--mux'], env={**env, 'PRISMATTYC_PLUGIN_BUNDLE_DIR': str(corrupt)}, capture_output=True, text=True, timeout=60)
            calls = [json.loads(line) for line in log.read_text().splitlines()]
            records.append({'case': 'corrupt archive', 'exit': proc.returncode, 'stderr': proc.stderr, 'calls': calls})
            assert proc.returncode != 0
            assert not any(call['program'] == 'cargo' and call['args'][:1] == ['install'] for call in calls), 'Corrupt bundle reached Cargo installation'
            assert 'before cargo installation' in proc.stderr, proc.stderr
            results.update(status='PASS', installs=paths, prepared_bundle='same verified pin for all three installs', corrupt_archive='rejected before install', native_flags='preserved and remapped')
    finally:
        (args.out / 'result.json').write_text(json.dumps(results, indent=2) + '\n')
        (args.out / 'commands.json').write_text(json.dumps(records, indent=2) + '\n')
    print('PASS verified source bundle; matched mux/MCP/thin-launcher install requests; corrupt archive rejected before installation')


if __name__ == '__main__':
    main()
