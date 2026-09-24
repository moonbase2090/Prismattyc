#!/usr/bin/env python3
"""Package and verify a complete native Windows executable set."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import sys
import struct
import subprocess
import tempfile
import zipfile

BINARIES = ('pmux', 'pmuxd', 'pmux-attach', 'pmux-mcp', 'prismattyc', 'prismattyc-host')


def digest(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def validate_pe(path):
    with path.open('rb') as stream:
        header = stream.read(64)
        if len(header) != 64 or header[:2] != b'MZ':
            raise ValueError(f'{path.name}: missing DOS executable header')
        stream.seek(struct.unpack_from('<I', header, 60)[0])
        pe = stream.read(26)
        if len(pe) != 26 or pe[:4] != b'PE\0\0' or struct.unpack_from('<H', pe, 4)[0] != 0x8664 or struct.unpack_from('<H', pe, 24)[0] != 0x20B:
            raise ValueError(f'{path.name}: expected a Windows x64 PE32+ executable')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--version', required=True)
    parser.add_argument('--target', choices=('x86_64-pc-windows-msvc', 'x86_64-pc-windows-gnu'), required=True)
    parser.add_argument('--bin-dir', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    args = parser.parse_args()
    if len(args.version.split('.')) != 3 or any(not p.isdecimal() for p in args.version.split('.')):
        parser.error('version must be three decimal components')
    if sys.platform != "win32":
        parser.error("package on native Windows so every executable version is checked")
    repo = Path(__file__).resolve().parents[2]
    bins = args.bin_dir.resolve()
    for name in BINARIES:
        path = bins / f'{name}.exe'
        validate_pe(path)
        result = subprocess.run([str(path), '--version'], capture_output=True, text=True, timeout=10, check=True)
        if args.version not in result.stdout.split():
            parser.error(f'{name}: unexpected version output {result.stdout!r}')
    args.out.mkdir(parents=True, exist_ok=False)
    revision = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=repo, text=True).strip()
    dirty = bool(subprocess.check_output(['git', 'status', '--porcelain', '--untracked-files=normal'], cwd=repo, text=True).strip())
    manifest = {'source_revision': revision, 'source_dirty': dirty, 'repository': 'moonbase2090/Prismattyc', 'version': args.version, 'target': args.target, 'assets': []}
    for name in BINARIES:
        target = args.out / f'prismattyc-v{args.version}-{args.target}-{name}.exe'
        shutil.copy2(bins / f'{name}.exe', target)
        manifest['assets'].append({'name': target.name, 'size': target.stat().st_size, 'sha256': digest(target)})
    (args.out / f'manifest-{args.target}.json').write_text(json.dumps(manifest, indent=2) + '\n', encoding='utf-8')
    shutil.copy2(repo / 'LICENSE', args.out / 'MPL-2.0.txt')
    shutil.copy2(repo / 'NOTICE.txt', args.out / 'NOTICE.txt')
    with tempfile.TemporaryDirectory() as temp:
        root = Path(temp) / f'prismattyc-{args.version}-windows-x64'
        (root / 'bin').mkdir(parents=True)
        (root / 'licenses').mkdir()
        for name in BINARIES:
            shutil.copy2(bins / f'{name}.exe', root / 'bin' / f'{name}.exe')
        for path in (repo / 'crates/prismattyc-host/assets/fonts').glob('*.txt'):
            shutil.copy2(path, root / 'licenses' / path.name)
        shutil.copy2(repo / 'LICENSE', root / 'licenses/MPL-2.0.txt')
        shutil.copy2(repo / 'NOTICE.txt', root / 'licenses/NOTICE.txt')
        shutil.copy2(repo / 'crates/prismattyc-host/themes/OMARCHY-LICENSE.txt', root / 'licenses/OMARCHY-LICENSE.txt')
        (root / 'VERSION').write_text(args.version + '\n', encoding='utf-8')
        (root / 'README.txt').write_text(
            'Prismattyc native Windows x64\n\n'
            'Requires Windows 10 version 1809 or newer (ConPTY), or Windows 11.\n'
            'Extract the complete archive into a user-owned directory.\n'
            'Launch bin\\prismattyc-host.exe for a window, or add bin to your user PATH.\n'
            'Use pmux.exe new NAME to create a persistent mux session.\n'
            'The default shell is COMSPEC (normally cmd.exe); pass -- powershell.exe\n'
            'or -- pwsh.exe to select PowerShell. Configuration is under APPDATA,\n'
            'and runtime/data files are under LOCALAPPDATA.\n'
            'Replacing or stopping pmuxd destroys active sessions; leave it running\n'
            'until you have closed them intentionally.\n'
            'Read licenses\\MPL-2.0.txt and licenses\\NOTICE.txt for licensing.\n', encoding='utf-8')
        files = sorted(p for p in root.rglob('*') if p.is_file())
        (root / 'SHA256SUMS').write_text(''.join(f'{digest(p)}  {p.relative_to(root).as_posix()}\n' for p in files), encoding='utf-8')
        with zipfile.ZipFile(args.out / f'prismattyc-v{args.version}-{args.target}.zip', 'w', zipfile.ZIP_DEFLATED) as archive:
            for path in sorted(p for p in root.rglob('*') if p.is_file()):
                archive.write(path, path.relative_to(root.parent).as_posix())
    files = sorted(p for p in args.out.iterdir() if p.is_file())
    (args.out / 'SHA256SUMS-windows').write_text(''.join(f'{digest(p)}  {p.name}\n' for p in files), encoding='utf-8')
    print(json.dumps(manifest, indent=2))


if __name__ == '__main__':
    main()
