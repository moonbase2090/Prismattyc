#!/usr/bin/env python3
"""Package a GitHub release. Linux targets write updater binaries and an archive.

aarch64-apple-darwin delegates to scripts/release/package-macos.sh, which
signs and notarizes the Apple silicon app. No git ancestry is required.
"""
import argparse
import hashlib
import json
import pathlib
import shutil
import subprocess
import tarfile
import tempfile

BINARIES = ('pmux', 'pmuxd', 'pmux-attach', 'pmux-mcp', 'prismattyc', 'prismattyc-host')
p = argparse.ArgumentParser(description=__doc__)
p.add_argument('--version', required=True)
p.add_argument('--target', required=True, choices=('x86_64-unknown-linux-gnu', 'aarch64-unknown-linux-gnu', 'x86_64-apple-darwin', 'aarch64-apple-darwin'))
p.add_argument('--bin-dir', type=pathlib.Path)
p.add_argument('--out', type=pathlib.Path, required=True)
p.add_argument('--man-dir', type=pathlib.Path)
a = p.parse_args()
repo = pathlib.Path(__file__).resolve().parents[2]

def sha256(path):
    digest = hashlib.sha256()
    with path.open('rb') as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b''):
            digest.update(chunk)
    return digest.hexdigest()
parts = a.version.split('.')
if len(parts) != 3 or any(not part.isdecimal() for part in parts) or tuple(map(int, parts)) < (0, 2, 0):
    p.error('use a stable version at or after 0.2.0')
if a.target.endswith('apple-darwin'):
    if a.target != 'aarch64-apple-darwin':
        p.error(
            'x86_64-apple-darwin is not packaged. There is no Intel or universal lipo build. '
            'Ship Apple silicon with scripts/release/package-macos.sh '
            '(Prismattyc-vVERSION-macos-arm64.zip)'
        )
    script = repo / 'scripts/release/package-macos.sh'
    cmd = ['bash', str(script), '--version', a.version, '--out', str(a.out)]
    if a.bin_dir is not None:
        cmd.extend(['--bin-dir', str(a.bin_dir)])
    raise SystemExit(subprocess.call(cmd))
if a.bin_dir is None or a.man_dir is None:
    p.error('Linux packaging requires --bin-dir and --man-dir')
a.bin_dir = a.bin_dir.resolve()
a.man_dir = a.man_dir.resolve()
architecture, elf_machine = {
    'x86_64-unknown-linux-gnu': ('x86_64', 62),
    'aarch64-unknown-linux-gnu': ('ARM64', 183),
}[a.target]
# Validate the entire set before publishing any package output.
for name in BINARIES:
    with (a.bin_dir / name).open('rb') as binary:
        header = binary.read(20)
    if (header[:6] != b'\x7fELF\x02\x01' or
            int.from_bytes(header[18:20], 'little') != elf_machine):
        p.error(f'{name} is not a Linux {architecture} executable')
    result = subprocess.run([str(a.bin_dir / name), '--version'], capture_output=True, timeout=5, check=True)
    if a.version not in result.stdout.decode().split():
        p.error(f'{name} has the wrong version')
for name in (*BINARIES, 'pmux-pane-write'):
    if not (a.man_dir / f'{name}.1').is_file():
        p.error(f'missing manual: {name}.1')
a.out.mkdir(parents=True, exist_ok=False)
shutil.copy2(repo / 'LICENSE', a.out / 'MPL-2.0.txt')
shutil.copy2(repo / 'NOTICE.txt', a.out / 'NOTICE.txt')
manifest = {'repository': 'moonbase2090/Prismattyc', 'version': a.version, 'target': a.target, 'assets': []}
for name in BINARIES:
    target = a.out / f'prismattyc-v{a.version}-{a.target}-{name}'
    shutil.copy2(a.bin_dir / name, target)
    digest = sha256(target)
    manifest['assets'].append({'name': target.name, 'size': target.stat().st_size, 'sha256': digest})
(a.out / f'manifest-{a.target}.json').write_text(json.dumps(manifest, indent=2) + '\n')
with tempfile.TemporaryDirectory() as tmp:
    root = pathlib.Path(tmp) / f'prismattyc-{a.version}'
    (root / 'bin').mkdir(parents=True)
    (root / 'share/man').mkdir(parents=True)
    (root / 'share/licenses').mkdir()
    shutil.copy2(repo / 'LICENSE', root / 'share/licenses/MPL-2.0.txt')
    shutil.copy2(repo / 'NOTICE.txt', root / 'share/licenses/NOTICE.txt')
    for notice in (repo / 'crates/prismattyc-host/assets/fonts').glob('*.txt'):
        shutil.copy2(notice, root / 'share/licenses' / notice.name)
    shutil.copy2(repo / 'crates/prismattyc-host/themes/OMARCHY-LICENSE.txt', root / 'share/licenses/OMARCHY-LICENSE.txt')
    for name in BINARIES:
        shutil.copy2(a.bin_dir / name, root / 'bin' / name)
    for name in (*BINARIES, 'pmux-pane-write'):
        shutil.copy2(a.man_dir / f'{name}.1', root / 'share/man' / f'{name}.1')
    shutil.copy2(repo / 'scripts/release/install.sh', root / 'install.sh')
    shutil.copy2(repo / 'assets/brand/prismattyc-icon-tile.svg', root / 'share/prismattyc.svg')
    (root / 'VERSION').write_text(a.version + '\n')
    (root / 'INSTALL.txt').write_text(
        f'Prismattyc for Linux {architecture} (Ubuntu 22.04 or newer).\n'
        'Install runtime dependencies with your distribution package manager:\n'
        'Ubuntu: sudo apt install libfontconfig1 libxkbcommon0 libxkbcommon-x11-0 libegl1\n'
        'Run ./install.sh to install into ~/.local, or use --prefix /absolute/path.\n'
        'Add ~/.local/bin to PATH. Launch prismattyc-host from the application menu.\n'
        'The installer does not stop running sessions. Reopen windows to use this version.\n'
        'Run pmux update for future releases. Read pmux update --help for rollback.\n')
    files = sorted(path for path in root.rglob('*') if path.is_file())
    (root / 'SHA256SUMS').write_text(''.join(f'{sha256(path)}  {path.relative_to(root)}\n' for path in files))
    archive = a.out / f'prismattyc-{a.target}.tar.gz'
    with tarfile.open(archive, 'w:gz') as tar:
        tar.add(root, arcname=root.name)
assets = sorted(path for path in a.out.iterdir() if path.is_file())
(a.out / 'SHA256SUMS').write_text(''.join(f'{sha256(path)}  {path.name}\n' for path in assets))
print(json.dumps(manifest, indent=2))
