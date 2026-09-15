#!/usr/bin/env python3
"""Package the complete executable set for a GitHub release. No git ancestry."""
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
p.add_argument('--bin-dir', type=pathlib.Path, required=True)
p.add_argument('--out', type=pathlib.Path, required=True)
p.add_argument('--man-dir', type=pathlib.Path, required=True)
a = p.parse_args()
a.bin_dir = a.bin_dir.resolve()
a.man_dir = a.man_dir.resolve()
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
# Validate the entire set before publishing any package output.
for name in BINARIES:
    result = subprocess.run([str(a.bin_dir / name), '--version'], capture_output=True, timeout=5, check=True)
    if a.version not in result.stdout.decode().split():
        p.error(f'{name} has the wrong version')
for name in (*BINARIES, 'pmux-pane-write'):
    if not (a.man_dir / f'{name}.1').is_file():
        p.error(f'missing manual: {name}.1')
if 'linux' not in a.target:
    p.error('macOS packaging requires the signed application release process')
a.out.mkdir(parents=True, exist_ok=False)
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
    shutil.copy2(repo / 'crates/prismattyc-host/themes/OMARCHY-LICENSE.txt', root / 'share/licenses/OMARCHY-LICENSE.txt')
    for name in BINARIES:
        shutil.copy2(a.bin_dir / name, root / 'bin' / name)
    for name in (*BINARIES, 'pmux-pane-write'):
        shutil.copy2(a.man_dir / f'{name}.1', root / 'share/man' / f'{name}.1')
    shutil.copy2(repo / 'scripts/release/install.sh', root / 'install.sh')
    shutil.copy2(repo / 'assets/brand/prismattyc-icon-tile.svg', root / 'share/prismattyc.svg')
    (root / 'VERSION').write_text(a.version + '\n')
    (root / 'INSTALL.txt').write_text(
        'Prismattyc for Linux x86_64 (Ubuntu 22.04 or newer).\n'
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
