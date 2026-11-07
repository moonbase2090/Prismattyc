#!/usr/bin/env python3
"""Smoke-test a built or installed release on one Unix machine.

Run this inside the clean environment for the platform: the macOS Tart guest,
the Linux container, or any disposable Linux or macOS account. It exercises the
shipped executables only, in a private daemon with its own socket and data
directories, and never touches an installed copy's state:

1. every shipped executable reports the same version (and the expected one);
2. real sessions and mail through the production commands;
3. saved Spaces through the production commands;
4. Update and Restart: a consented daemon restart restores each pane running
   its program, then a stopped daemon reopens the Space.

The Windows package has its own installer proof; see docs/release-smoke.md.
"""
import argparse
import json
import platform
import re
import subprocess
import sys
import time
from pathlib import Path

sys.dont_write_bytecode = True

CORE = ('pmux', 'pmuxd', 'pmux-attach', 'pmux-mcp', 'prismattyc', 'prismattyc-host')
OPTIONAL = ('pmux-activity-monitor',)
HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1] if len(HERE.parents) > 1 else HERE


def find(name, candidates):
    """First existing file: next to this script (a flat copy) or in the repo."""
    for path in candidates:
        if path.is_file():
            return path
    raise SystemExit(f'missing helper {name}; copy it next to this script or run from a checkout')


def version_of(binary):
    out = subprocess.run([str(binary), '--version'], capture_output=True, text=True, timeout=30)
    if out.returncode != 0:
        raise AssertionError(f'{binary.name} --version exited {out.returncode}: {out.stderr.strip()}')
    match = re.search(r'\b(\d+\.\d+\.\d+(?:-[0-9A-Za-z.]+)?)\b', out.stdout + out.stderr)
    if not match:
        raise AssertionError(f'{binary.name} reported no version: {out.stdout!r}')
    return match.group(1)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--bins', type=Path, required=True, help='directory holding the shipped executables')
    parser.add_argument('--out', type=Path, required=True, help='new directory for evidence')
    parser.add_argument('--expect-version', help='version every executable must report, such as 0.3.58')
    args = parser.parse_args()
    bins = args.bins.resolve()
    args.out.mkdir(parents=True, exist_ok=False)

    verify_cli = find('verify-cli.py', [HERE / 'verify-cli.py',
                                        REPO / '.cursor/skills/verify-prismattyc/scripts/verify-cli.py'])
    update_restart = find('update-restart-e2e.py', [HERE / 'update-restart-e2e.py',
                                                    REPO / 'tests/native/update-restart-e2e.py'])
    result = dict(platform=f'{platform.system()} {platform.release()} {platform.machine()}',
                  bins=str(bins), steps=[], status='FAIL')

    def step(name, action):
        started = time.monotonic()
        record = dict(name=name)
        result['steps'].append(record)
        try:
            record['detail'] = action()
            record['status'] = 'PASS'
        except (AssertionError, subprocess.SubprocessError, OSError) as error:
            record.update(status='FAIL', error=str(error)[-2000:])
            raise
        finally:
            record['seconds'] = round(time.monotonic() - started, 1)
            print(f"{record.get('status', 'FAIL')} {name} ({record['seconds']}s)", flush=True)

    def versions():
        found = {}
        for name in CORE:
            path = bins / name
            assert path.is_file(), f'missing shipped executable: {path}'
            found[name] = version_of(path)
        for name in OPTIONAL:
            if (bins / name).is_file():
                found[name] = version_of(bins / name)
        assert len(set(found.values())) == 1, f'executables disagree on the version: {found}'
        if args.expect_version:
            assert set(found.values()) == {args.expect_version}, (args.expect_version, found)
        return found

    def drive(label, script, *extra):
        def run():
            proc = subprocess.run([sys.executable, '-I', str(script), *extra], capture_output=True, text=True, timeout=900)
            (args.out / f'{label}.log').write_text(proc.stdout + proc.stderr)
            assert proc.returncode == 0, (proc.stdout + proc.stderr)[-2000:]
            return (proc.stdout.strip().splitlines() or [''])[-1][:300]
        return run

    try:
        step('versions agree', versions)
        step('sessions and mail', drive('sessions-mail', verify_cli, '--bins', str(bins), '--out', str(args.out / 'sessions-mail'),
                                        '--feature', 'sessions-mail'))
        step('saved spaces', drive('spaces', verify_cli, '--bins', str(bins), '--out', str(args.out / 'spaces'),
                                   '--feature', 'spaces'))
        step('update and restart, space reopen', drive('update-restart', update_restart, '--bins', str(bins),
                                                       '--out', str(args.out / 'update-restart')))
        result['status'] = 'PASS'
        print('SMOKE_RELEASE_PASS: ' + ', '.join(s['name'] for s in result['steps']), flush=True)
        return 0
    except (AssertionError, subprocess.SubprocessError, OSError):
        return 1
    finally:
        (args.out / 'smoke.json').write_text(json.dumps(result, indent=2) + '\n')


if __name__ == '__main__':
    raise SystemExit(main())
