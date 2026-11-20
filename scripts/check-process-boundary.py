#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Check the shared child-launch boundary and the host's dependency closure."""
import argparse
import difflib
import json
import os
from pathlib import Path
import re
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
BOUNDARY = Path('crates/prismattyc-process/src/lib.rs')
AUDIT = ROOT / '.config/process-runtime-audit.json'
RAW_LAUNCH = re.compile(r'\bfn\s+(?:fork|vfork|posix_spawnp?|exec(?:l|le|lp|v|ve|vp|vpe|veat)|fexecve|forkpty|system|popen|syscall)\s*\(')
SUPPRESSION = re.compile(r'#\s*!?\s*\[[^]]*?\b(?:allow|expect)\s*\([^]]*\b(?:disallowed_methods|all|restriction|style|warnings)\b[^]]*\]', re.S)
TARGETS = ('aarch64-apple-darwin', 'x86_64-apple-darwin', 'aarch64-unknown-linux-gnu', 'x86_64-unknown-linux-gnu')


def source_errors(root=ROOT):
    errors = []
    for directory in ('crates', 'third_party/portable-pty', 'tools/activity-monitor'):
        for path in sorted((root / directory).rglob('*.rs')):
            relative = path.relative_to(root)
            if 'target' in relative.parts:
                continue
            source = path.read_text(encoding="utf-8")
            # Raw launch declarations are forbidden even behind inactive cfg.
            for match in RAW_LAUNCH.finditer(source):
                errors.append(f'{relative}:{source.count(chr(10), 0, match.start()) + 1}: raw launch declaration; use prismattyc_process')
            if re.search(r'#\s*\[\s*link_name\s*=\s*"(?:fork|vfork|posix_spawnp?|exec[^"]*|system|popen|syscall|forkpty)"\s*\]', source):
                errors.append(f'{relative}: renamed raw launch declaration; use prismattyc_process')
            suppressions = SUPPRESSION.findall(source)
            expected = [
                '#[expect(clippy::disallowed_methods, reason = "sole process spawn boundary")]',
                '#[expect(clippy::disallowed_methods, reason = "sole process exec boundary")]',
            ] if relative == BOUNDARY else []
            if suppressions != expected:
                errors.append(f'{relative}: unexpected process-boundary lint suppression')
    return errors


def cargo(*arguments, capture=False):
    command = [os.environ.get('CARGO', 'cargo'), *arguments]
    result = subprocess.run(command, cwd=ROOT, text=True, stdout=subprocess.PIPE if capture else None, check=True)
    return result.stdout if capture else None


def runtime_audit(target):
    metadata = json.loads(cargo('metadata', '--format-version=1', '--locked', '--filter-platform', target, capture=True))
    packages = {package['id']: package for package in metadata['packages']}
    nodes = {node['id']: node for node in metadata['resolve']['nodes']}
    roots = [key for key, package in packages.items() if package['name'] == 'prismattyc-host']
    if len(roots) != 1:
        raise ValueError('expected one desktop host package')
    pending, visited, records = roots[:], set(), []
    while pending:
        identity = pending.pop()
        if identity in visited:
            continue
        visited.add(identity)
        package, node = packages[identity], nodes[identity]
        source = package['source']
        if source is None:
            source = 'path:' + str(Path(package['manifest_path']).resolve().parent.relative_to(ROOT.resolve()))
        records.append({'name': package['name'], 'version': package['version'], 'source': source, 'features': sorted(node['features'])})
        # metadata is filtered for the requested target. Audit only normal
        # dependency edges, including the conservative resolved feature union.
        for dependency in node['deps']:
            if any(kind['kind'] is None for kind in dependency['dep_kinds']):
                pending.append(dependency['pkg'])
    boundary = [record for record in records if record['name'] == 'prismattyc-process']
    if len(boundary) != 1 or boundary[0]['source'] != 'path:crates/prismattyc-process':
        raise ValueError(f'{target}: host must resolve exactly one local process boundary')
    return sorted(records, key=lambda record: (record['name'], record['version'], record['source']))


def dependency_lint(package, profile, cwd=ROOT, capture=False):
    # cargo-clippy's workspace wrapper skips excluded path dependencies.
    # An ordinary driver wrapper also checks the selected excluded library.
    driver = subprocess.check_output(['rustup', 'which', 'clippy-driver'], text=True).strip()
    env = dict(os.environ, RUSTC_WRAPPER=driver, CLIPPY_CONF_DIR=str(ROOT))
    env.pop('RUSTC_WORKSPACE_WRAPPER', None)
    command = [os.environ.get('CARGO', 'cargo'), 'rustc', '-p', package, '--lib', '--locked', *profile,
               '--', '--emit=metadata', '-Aclippy::all', '-Dclippy::disallowed_methods']
    return subprocess.run(command, cwd=cwd, env=env, text=True, capture_output=capture)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source-only', action='store_true')
    parser.add_argument('--audit-only', action='store_true')
    parser.add_argument('--write-audit', action='store_true', help='regenerate for dependency review; does not approve changes')
    parser.add_argument('--target', choices=TARGETS, help='check one target audit instead of the native target')
    parser.add_argument('--release', action='store_true')
    args = parser.parse_args()
    if args.target and not args.audit_only:
        parser.error('--target selects an audit and requires --audit-only')
    errors = source_errors()
    if errors:
        print('\n'.join(errors), file=sys.stderr)
        return 1
    print('PASS process-boundary source declarations and suppressions', flush=True)
    if args.source_only:
        return 0
    if args.write_audit:
        data = {target: runtime_audit(target) for target in TARGETS}
        AUDIT.write_text('{\n' + ',\n'.join('  ' + json.dumps(target) + ': [\n' + ',\n'.join('    ' + json.dumps(record, separators=(",", ":")) for record in records) + '\n  ]' for target, records in data.items()) + '\n}\n')
        print('Wrote dependency audit; review changed sources and features before accepting it.')
        return 0
    target = args.target
    if target is None:
        rustc = subprocess.check_output(['rustc', '-vV'], text=True)
        target = re.search(r'^host: (.+)$', rustc, re.M).group(1)
    if target in TARGETS:
        expected = json.loads(AUDIT.read_text())[target]
        actual = runtime_audit(target)
        if actual != expected:
            print('Host runtime dependencies changed. Audit launch implementations, then regenerate --write-audit.', file=sys.stderr)
            print(''.join(difflib.unified_diff(json.dumps(expected, indent=2).splitlines(True), json.dumps(actual, indent=2).splitlines(True), fromfile='reviewed', tofile='resolved')), file=sys.stderr)
            return 1
        print(f'PASS {target} host dependency closure and single process boundary', flush=True)
    if args.audit_only:
        return 0
    profile = ['--release'] if args.release else []
    cargo('clippy', '--workspace', '--all-targets', '--locked', *profile, '--', '-Dwarnings', '-Dclippy::disallowed_methods')
    dependency_lint('portable-pty', profile).check_returncode()
    cargo('clippy', '--manifest-path', 'tools/activity-monitor/Cargo.toml', '--all-targets', '--locked', *profile, '--', '-Dwarnings', '-Dclippy::disallowed_methods')
    print('PASS type-resolved process-boundary lint')
    return 0


if __name__ == '__main__':
    sys.exit(main())
