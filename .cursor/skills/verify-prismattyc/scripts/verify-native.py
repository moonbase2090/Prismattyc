#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Build the matching test client and drive isolated native transport windows."""
import argparse
import hashlib
import json
import os
import platform as platform_info
from pathlib import Path
import subprocess
import sys


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bins', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--probe', type=Path, help='Use a previously built matching test client.')
    parser.add_argument('--case', default='all', choices=('all', 'images', 'off', 'legacy', 'resize', 'text', 'text-no-mouse', 'ctrl-z', 'lease', 'budgets', 'animation', 'metrics'))
    parser.add_argument('--presenter', choices=('tiles', 'iosurface', 'both'), default='both')
    args = parser.parse_args()
    scripts = Path(__file__).resolve().parent
    repo = scripts.parents[3]
    platform = 'macos' if sys.platform == 'darwin' else 'linux' if sys.platform == 'linux' else None
    if platform is None:
        parser.error('Requires supported native Linux or Apple Silicon macOS; other platforms use ANSI.')
    if platform == 'macos' and platform_info.machine() != 'arm64':
        parser.error('The macOS native fixture requires a native Apple Silicon process.')
    cases = ['images', 'off', 'ctrl-z', 'lease', 'budgets', 'animation', 'metrics', 'text'] if platform == 'macos' else ['images', 'legacy', 'off', 'resize', 'text', 'text-no-mouse']
    if args.case != 'all':
        if args.case not in cases:
            parser.error(f'{args.case} has no {platform} fixture; see the desktop verification map.')
        cases = [args.case]
    bins, out = args.bins.resolve(), args.out.resolve()
    for binary in ('prismattyc-host', 'pmuxd'):
        if not (bins / binary).is_file():
            parser.error(f'Missing matching binary: {binary}')
    out.mkdir(parents=True, exist_ok=False)
    report = {'status': 'FAIL', 'platform': platform, 'architecture': platform_info.machine(), 'cases': []}
    try:
        revision = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=repo, text=True).strip()
        dirty = bool(subprocess.check_output(['git', 'status', '--porcelain'], cwd=repo))
        report.update(revision=revision, worktree_dirty=dirty)
        versions = {name: subprocess.check_output([str(bins / name), '--version'], text=True).strip() for name in ('prismattyc-host', 'pmuxd')}
        assert all(revision[:12] in version for version in versions.values()), 'host and daemon must embed the current source revision'
        report['binary_versions'] = versions
        if args.probe:
            probe = args.probe.resolve()
            assert probe.is_file(), 'provided test client does not exist'
        else:
            target = out / 'client-target'
            env = dict(os.environ, CARGO_TARGET_DIR=str(target), NATIVE_PROBE_REVISION=revision)
            command = ['cargo', '+1.90.0', 'build', '--release', '--locked', '--manifest-path', str(scripts / 'native-client/Cargo.toml')]
            with (out / 'client-build.log').open('w') as log:
                subprocess.run(command, cwd=repo, env=env, stdout=log, stderr=subprocess.STDOUT, check=True)
            probe = target / 'release/native-client-probe'
        probe_version = subprocess.check_output([str(probe), '--version'], text=True).strip()
        assert revision in probe_version, 'test client must embed the matching source revision'
        report['probe_version'] = probe_version
        report['binary_sha256'] = {path.name: hashlib.sha256(path.read_bytes()).hexdigest() for path in [bins / 'prismattyc-host', bins / 'pmuxd', probe]}
        presenters = ['tiles', 'iosurface'] if args.presenter == 'both' else [args.presenter]
        for case in cases:
            for presenter in presenters if platform == 'macos' else [None]:
                name = case + (('-' + presenter) if presenter else '')
                evidence = out / name
                command = [sys.executable, str(scripts / f'verify-native-{platform}.py'), '--repo', str(repo), '--bins', str(bins), '--probe', str(probe), '--out', str(evidence)]
                if platform == 'macos':
                    lifecycle = 'images' if case == 'off' else 'metrics-live' if case == 'metrics' else case
                    command += ['--presenter', presenter, '--native', 'off' if case == 'off' else 'on', '--lifecycle', lifecycle]
                    if case in ('animation', 'metrics'):
                        command += ['--window-cells', '12x8' if case == 'animation' else '24x12']
                    elif case == 'text':
                        command += ['--window-cells', '40x10']
                else:
                    command += ['--mode', case]
                with (out / (name + '.log')).open('w') as log:
                    completed = subprocess.run(command, cwd=repo, stdout=log, stderr=subprocess.STDOUT)
                result_path = evidence / 'result.json'
                result = json.loads(result_path.read_bytes()) if result_path.exists() else {'status': 'FAIL', 'error': 'driver did not retain result.json'}
                report['cases'].append({'name': name, 'exit': completed.returncode, 'status': result['status'], 'evidence': name})
                if result['status'] == 'BLOCKED':
                    report.update(status='BLOCKED', blocked_case=name, prerequisite=result.get('prerequisite'))
                    raise SystemExit(2)
                assert completed.returncode == 0 and result['status'] == 'PASS', f'{name} failed; inspect its retained log and result'
                print(name, 'PASS', flush=True)
        report['status'] = 'PASS'
    except SystemExit:
        raise
    except BaseException as error:
        report['error'] = repr(error)
        raise
    finally:
        (out / 'result.json').write_text(json.dumps(report, indent=2) + '\n')


if __name__ == '__main__':
    main()
