#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Exercise the launch lint with real Rust name resolution and raw FFI fixtures."""
import importlib.util
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('process_boundary', ROOT / 'scripts/check-process-boundary.py')
guard = importlib.util.module_from_spec(spec)
spec.loader.exec_module(guard)


def main():
    with tempfile.TemporaryDirectory(prefix='process-boundary-lint-') as temporary:
        root = Path(temporary)
        (root / 'src').mkdir()
        (root / 'Cargo.toml').write_text('[package]\nname="boundary-lint-fixture"\nversion="0.0.0"\nedition="2021"\n[workspace]\n[dependencies]\nlibc="0.2"\n')
        shutil.copyfile(ROOT / 'Cargo.lock', root / 'Cargo.lock')
        env = dict(os.environ, CLIPPY_CONF_DIR=str(ROOT))
        env.pop('CARGO_ENCODED_RUSTFLAGS', None)
        source = root / 'src/lib.rs'
        negative = {
            'spawn': 'let _ = std::process::Command::new("child").spawn();',
            'output': 'let _ = std::process::Command::new("child").output();',
            'status': 'let _ = std::process::Command::new("child").status();',
            'alias': 'use std::process::Command as Alias; let _ = Alias::new("child").spawn();',
            'ufcs': 'let _ = std::process::Command::spawn(&mut std::process::Command::new("child"));',
        }
        if os.name == 'posix':
            negative.update({
                'exec': 'use std::os::unix::process::CommandExt; let _ = std::process::Command::new("child").exec();',
                'exec-alias-ufcs': 'use std::os::unix::process::CommandExt as Alias; let _ = Alias::exec(&mut std::process::Command::new("child"));',
                'fork-alias': 'use libc::fork as launch; let _ = unsafe { launch() };',
            })
        fixtures = [('thread-positive', 'let _ = std::thread::Builder::new().spawn(|| ());', True)]
        fixtures.extend((name, body, False) for name, body in negative.items())
        for name, body, passing in fixtures:
            source.write_text('pub fn probe() { ' + body + ' }\n')
            result = subprocess.run([os.environ.get('CARGO', 'cargo'), 'clippy', '--offline', '--manifest-path', str(root / 'Cargo.toml'), '--target-dir', str(root / 'target'), '--', '-Dclippy::disallowed_methods'], env=env, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
            if passing:
                assert result.returncode == 0, result.stdout
            else:
                assert result.returncode != 0 and 'use of a disallowed method' in result.stdout, result.stdout
            print(f'PASS {name}', flush=True)
        # Prove the same invocation also lints an excluded path dependency.
        excluded = root / 'excluded'
        excluded.mkdir()
        (excluded / 'Cargo.toml').write_text('[package]\nname="excluded-launch-fixture"\nversion="0.0.0"\nedition="2021"\n[lib]\npath="lib.rs"\n')
        manifest = root / 'Cargo.toml'
        manifest.write_text(manifest.read_text().replace('[workspace]\n', '[workspace]\nexclude=["excluded"]\n') + 'excluded-launch-fixture={path="excluded"}\n')
        source.write_text('pub fn probe() { excluded_launch_fixture::probe(); }\n')
        for body, passing in (('std::thread::spawn(|| ());', True), ('let _ = std::process::Command::new("child").spawn();', False)):
            (excluded / 'lib.rs').write_text('pub fn probe() { ' + body + ' }\n')
            subprocess.run([os.environ.get('CARGO', 'cargo'), 'metadata', '--offline', '--format-version=1'], cwd=root, stdout=subprocess.DEVNULL, check=True)
            result = guard.dependency_lint('excluded-launch-fixture', [], cwd=root, capture=True)
            diagnostic = result.stdout + result.stderr
            assert (result.returncode == 0) == passing, diagnostic
            if not passing:
                assert 'use of a disallowed method' in diagnostic, diagnostic
        print('PASS excluded dependency positive and bypass controls', flush=True)
        raw = root / 'crates/raw/src'
        raw.mkdir(parents=True)
        source = raw / 'lib.rs'
        source.write_bytes('// \u4f41\nfn ordinary() {}\n'.encode('utf-8'))
        assert not guard.source_errors(root)
        print('PASS UTF-8 source under platform text defaults', flush=True)
        for name, text in (
            ('raw-ffi', 'unsafe extern "C" { fn fork() -> i32; }'),
            ('renamed-ffi', 'unsafe extern "C" { #[link_name="fork"] fn launch() -> i32; }'),
            ('suppression', '#[allow(clippy::disallowed_methods)] fn hidden() {}'),
            ('group-suppression', '#[allow(clippy::all)] fn hidden() {}'),
            ('conditional-suppression', '#[cfg_attr(unix, allow(clippy::disallowed_methods))] fn hidden() {}'),
        ):
            source.write_text(text)
            assert guard.source_errors(root), name
            print(f'PASS {name}', flush=True)
    print('PASS launch lint positive and negative controls')


if __name__ == '__main__':
    main()
