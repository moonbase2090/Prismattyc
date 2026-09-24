#!/usr/bin/env python3
"""Assemble an explicitly cross-compiled Windows dogfood archive."""
import argparse
import importlib.util
import json
import re
from pathlib import Path
import shutil
import subprocess
import tempfile
import tomllib
import zipfile

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--repo', type=Path, required=True)
parser.add_argument('--bins', type=Path, required=True)
parser.add_argument('--out', type=Path, required=True)
args = parser.parse_args()
repo = args.repo.resolve()
if Path(__file__).resolve().parent != repo / 'scripts/release':
    parser.error('--repo must be the checkout containing this packager')
revision = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=repo, text=True).strip()
if subprocess.check_output(['git', 'status', '--porcelain'], cwd=repo, text=True).strip():
    parser.error('source checkout must be clean')
if not re.fullmatch(r'[0-9a-f]{40}', revision):
    parser.error('expected a full Git SHA-1 revision')
version = tomllib.loads((repo / 'Cargo.toml').read_text())['workspace']['package']['version']
spec = importlib.util.spec_from_file_location('windows_release', repo / 'scripts/release/package-windows.py')
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)
args.out.mkdir(parents=True, exist_ok=False)
label = f'prismattyc-windows-x64-{revision}-preview'
with tempfile.TemporaryDirectory(dir=args.out) as temp:
    root = Path(temp) / label
    (root / 'bin').mkdir(parents=True)
    (root / 'licenses').mkdir()
    for name in release.BINARIES:
        path = root / 'bin' / f'{name}.exe'
        shutil.copy2(args.bins / f'{name}.exe', path)
        release.validate_pe(path)
        if revision[:12].encode() not in path.read_bytes():
            parser.error(f'{name}: source revision not embedded in binary')
    shutil.copy2(Path(__file__).with_name('install-windows-preview.ps1'), root / 'install.ps1')
    for name, source in [('MPL-2.0.txt', repo / 'LICENSE'), ('NOTICE.txt', repo / 'NOTICE.txt'), ('OMARCHY-LICENSE.txt', repo / 'crates/prismattyc-host/themes/OMARCHY-LICENSE.txt')]:
        shutil.copy2(source, root / 'licenses' / name)
    for source in (repo / 'crates/prismattyc-host/assets/fonts').glob('*.txt'):
        shutil.copy2(source, root / 'licenses' / source.name)
    (root / 'SOURCE.txt').write_text(f'Prismattyc {version}\nSource revision: {revision}\nhttps://github.com/moonbase2090/Prismattyc/tree/{revision}\nBuild target: x86_64-pc-windows-gnu\nNative Windows execution before packaging: not performed\nDistribution: unsigned cross-compiled preview; native runtime unverified\n', encoding='utf-8')
    (root / 'README.txt').write_text('''Prismattyc native Windows x64 preview

Requires Windows 10 version 1809 or newer, or Windows 11.
This unsigned development build was cross-compiled on Linux. Native Windows
runtime and performance have not been verified. Installer version probes do not
validate interactive behavior or performance.

1. Extract the whole ZIP.
2. Open PowerShell in the extracted directory.
3. Run:
   powershell -NoProfile -ExecutionPolicy Bypass -File .\\install.ps1 -AddToPath

No administrator access is needed. The installer verifies package checksums
and runs --version on all six programs before copying them into:
%LOCALAPPDATA%\\Programs\\Prismattyc\\windows-preview-<revision>
It creates a Start menu shortcut named Prismattyc Windows Preview <revision>.
-AddToPath adds this build's bin directory to your user PATH. Omit that flag
for a shortcut-only installation. Reopen your terminal after changing PATH.

Launch the revision-specific Prismattyc Windows Preview shortcut from Start, or run bin\\prismattyc-host.exe
from the extracted archive for portable use. The default shell is cmd.exe.
To use PowerShell: prismattyc-host.exe -- powershell.exe
To create a persistent session: pmux.exe new windows-dogfood

The installer does not start, stop, or replace running host/daemon processes.
No running executable is overwritten. Configuration uses APPDATA and runtime
files use LOCALAPPDATA. Closing a desktop window and stopping pmuxd are distinct:
stopping the daemon destroys its live sessions.

Keep this exact revision in any dogfood report. Do not use this preview ZIP
as an immutable production-release asset. The complete source archive is source.tar.gz. Source and licensing are described
in SOURCE.txt and licenses. Checksums detect corruption; this is not a signed
Windows distribution.
''', encoding='utf-8')
    subprocess.run(['git', 'archive', '--format=tar.gz',
                    f'--prefix=prismattyc-{revision}/',
                    f'--output={(root / "source.tar.gz").resolve()}', revision],
                   cwd=repo, check=True)
    files = []
    for path in sorted(p for p in root.rglob('*') if p.is_file()):
        files.append({'path': path.relative_to(root).as_posix(), 'sha256': release.digest(path), 'size': path.stat().st_size})
    if subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=repo, text=True).strip() != revision or subprocess.check_output(['git', 'status', '--porcelain'], cwd=repo, text=True).strip():
        parser.error('source checkout changed during packaging')
    manifest = {'version': version, 'source_revision': revision, 'target': 'x86_64-pc-windows-gnu', 'kind': 'dogfood-preview', 'native_runtime_verified': False, 'signed': False, 'cross_compiled': True, 'files': files}
    (root / 'manifest.json').write_text(json.dumps(manifest, indent=2) + '\n', encoding='utf-8')
    archive_path = args.out / f'{label}.zip'
    with zipfile.ZipFile(archive_path, 'w', zipfile.ZIP_DEFLATED) as archive:
        for path in sorted(p for p in root.rglob('*') if p.is_file()):
            archive.write(path, path.relative_to(root.parent).as_posix())
(args.out / 'SHA256SUMS').write_text(''.join(f'{release.digest(path)}  {path.name}\n' for path in sorted(args.out.iterdir()) if path.is_file()), encoding='utf-8')
print(json.dumps({'source_revision': revision, 'archive': str(archive_path.resolve()), 'sha256': release.digest(archive_path)}, indent=2))
