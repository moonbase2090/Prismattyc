#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Prepare a verified bundle or compare its identity with built native consumers."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import sys
import subprocess

REPO = Path(__file__).resolve().parents[2]
CONSUMERS = ('pmux', 'pmuxd', 'pmux-mcp', 'prismattyc-host', 'pmux-activity-monitor')


def prepare():
    command = [sys.executable, str(REPO / 'scripts/release/prepare-plugin-bundle.py')]
    command.extend(['--target-dir', os.environ.get('CARGO_TARGET_DIR', str(REPO / 'target'))])
    explicit = os.environ.get('PRISMATTYC_PLUGIN_BUNDLE_DIR')
    if explicit:
        source = Path(explicit).resolve()
        pin = json.loads((source / 'bundle-pin.json').read_bytes())
        command.extend(['--candidate' if pin['source']['kind'] == 'candidate' else '--bundle', str(source)])
    return subprocess.check_output(command, cwd=REPO, text=True).strip()


def bundle_path(bundle=None):
    value = bundle or os.environ.get('PRISMATTYC_PLUGIN_BUNDLE_DIR')
    if not value:
        raise ValueError('prepare a verified plug-in bundle before building or packaging')
    return Path(value).resolve()


def expected_identity(bundle):
    pin_bytes = (bundle / 'bundle-pin.json').read_bytes()
    pin = json.loads(pin_bytes)
    catalog_bytes = (bundle / 'pmux-plugins.json').read_bytes()
    receipt_bytes = (bundle / 'plugin-build-receipt.json').read_bytes()
    for label, data in [('catalog', catalog_bytes), ('receipt', receipt_bytes)]:
        if len(data) != pin[f'{label}_bytes'] or hashlib.sha256(data).hexdigest() != pin[f'{label}_sha256']:
            raise ValueError(f'prepared bundle {label} differs from its pin')
    packages = json.loads(catalog_bytes)['packages']
    if {entry['id'] for entry in packages} != {'org.moonbase.activity-monitor', 'org.moonbase.activity-connector'}:
        raise ValueError('packaging requires the real production plug-ins')
    return {
        'version': 1,
        'catalog_sha256': pin['catalog_sha256'],
        'pin_sha256': hashlib.sha256(pin_bytes).hexdigest(),
        'receipt_sha256': pin['receipt_sha256'],
        'packages': sorted(packages, key=lambda entry: (entry['id'], entry['version'])),
    }


def verify_binaries(bins, bundle=None):
    bundle = bundle_path(bundle)
    expected = expected_identity(bundle)
    revision = subprocess.check_output(['git', 'rev-parse', '--short=12', 'HEAD'], cwd=REPO, text=True).strip()
    suffix = '.exe' if os.name == 'nt' else ''
    matched = None
    for name in CONSUMERS:
        data = subprocess.check_output([str(Path(bins).resolve() / (name + suffix)), '--plugin-bundle'], text=True, timeout=10)
        identity = json.loads(data)
        if identity.get('core_revision') != revision:
            raise ValueError(f'{name}: executable source revision differs from this checkout')
        actual = {key: identity.get(key) for key in expected}
        actual['packages'] = sorted(actual['packages'], key=lambda entry: (entry['id'], entry['version']))
        if actual != expected:
            raise ValueError(f'{name}: embedded plug-in bundle differs from the prepared bundle')
        if matched is not None and identity != matched:
            raise ValueError(f'{name}: plug-in consumers were built with different identities')
        matched = identity
    return {
        'identity': matched,
        'pin': json.loads((bundle / 'bundle-pin.json').read_bytes()),
        'build_receipt': json.loads((bundle / 'plugin-build-receipt.json').read_bytes()),
    }


def native_build_flags(target=None, crt_static=False):
    encoded = os.environ.get('CARGO_ENCODED_RUSTFLAGS')
    if encoded is not None:
        flags = encoded.split('\x1f') if encoded else []
    else:
        fallback = os.environ.get('CARGO_TARGET_' + target.upper().replace('-', '_') + '_RUSTFLAGS', '') if target else ''
        flags = os.environ.get('RUSTFLAGS', fallback).split()
    if crt_static:
        flags.extend(['-C', 'target-feature=+crt-static'])
    roots = [
        (Path.home(), '/build-home'),
        (REPO, '/workspace'),
        (Path(os.environ.get('CARGO_HOME', Path.home() / '.cargo')), '/cargo-home'),
        (Path(os.environ.get('CARGO_TARGET_DIR', REPO / 'target')), '/build-target'),
    ]
    for source, replacement in roots:
        flags.extend(['--remap-path-prefix', f'{source.resolve()}={replacement}'])
    return '\x1f'.join(flags)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('operation', choices=['prepare', 'verify', 'flags'])
    parser.add_argument('--bins', type=Path)
    parser.add_argument('--bundle', type=Path)
    parser.add_argument('--out', type=Path)
    parser.add_argument('--target')
    parser.add_argument('--crt-static', action='store_true')
    args = parser.parse_args()
    if args.operation == 'flags':
        print(native_build_flags(args.target, args.crt_static))
        return
    if args.target or args.crt_static:
        parser.error('--target and --crt-static apply only to flags')
    if args.operation == 'prepare':
        if args.bins or args.bundle or args.out:
            parser.error('prepare uses the reviewed pin or explicit PRISMATTYC_PLUGIN_BUNDLE_DIR')
        print(prepare())
        return
    if args.bins is None:
        parser.error('verify requires --bins')
    proof = verify_binaries(args.bins, args.bundle)
    text = json.dumps(proof, indent=2, sort_keys=True) + '\n'
    if args.out:
        args.out.write_text(text)
    else:
        print(text, end='')


if __name__ == '__main__':
    main()
