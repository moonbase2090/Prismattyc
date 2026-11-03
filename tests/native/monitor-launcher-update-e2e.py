#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Drive an actual old six-binary updater, then explicit contained-launcher repair.

Run on native Linux or Windows with seven matched new binaries and a verified
older official pmux executable. Only HTTPS transport is replaced by a local curl
fixture. No production release, installation, daemon, or session is changed.
"""
import argparse
from contextlib import contextmanager
import hashlib
import http.client
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request

CORE = ('pmux', 'pmuxd', 'pmux-attach', 'pmux-mcp', 'prismattyc', 'prismattyc-host')
MONITOR = 'pmux-activity-monitor'
REPOSITORY = 'moonbase2090/Prismattyc'


# Attempts for one GitHub request, and the longest single wait between them.
FETCH_ATTEMPTS = 5
FETCH_MAX_WAIT_SECONDS = 60


def retry_wait(error, attempt, now=None):
    """Seconds to wait before retrying `error`, or None to give up.

    Retries rate limits (429, or 403 with `Retry-After` or no remaining
    quota) and server errors. Any other 403 is a real refusal.
    """
    if not isinstance(error, urllib.error.HTTPError):
        return min(2 ** attempt, FETCH_MAX_WAIT_SECONDS)
    headers = error.headers or {}
    limited = error.code == 429 or (
        error.code == 403
        and (headers.get('Retry-After') is not None or headers.get('x-ratelimit-remaining') == '0')
    )
    if not limited and error.code < 500:
        return None
    retry_after = headers.get('Retry-After')
    if retry_after is not None and retry_after.strip().isdigit():
        wait = int(retry_after)
    elif headers.get('x-ratelimit-reset', '').strip().isdigit():
        now = time.time() if now is None else now
        wait = int(headers['x-ratelimit-reset']) - int(now) + 1
    else:
        wait = 2 ** attempt
    return max(1, min(wait, FETCH_MAX_WAIT_SECONDS))


# Failures that end one attempt but may pass on the next: the request, a
# reset or timeout while reading the body, or a body cut short.
RETRYABLE = (urllib.error.URLError, ConnectionError, TimeoutError, http.client.HTTPException)


def with_retry(label, attempt):
    """Run `attempt()` until it succeeds, retrying what `retry_wait` allows.

    `attempt` does the whole request, including reading the body, so a
    failure part way through the read is retried too.
    """
    for number in range(FETCH_ATTEMPTS):
        try:
            return attempt()
        except RETRYABLE as error:
            wait = retry_wait(error, number)
            if wait is None or number + 1 == FETCH_ATTEMPTS:
                raise
            if isinstance(error, urllib.error.HTTPError):
                error.close()
            print(f'retrying {label} in {wait}s: {error}', file=sys.stderr)
            time.sleep(wait)
    raise AssertionError('unreachable')


def api_request(url):
    """A GitHub API request, authenticated when a token is set.

    Anonymous API calls share a small per-IP quota on CI runners. The token is
    an unredirected header: urllib never copies it onto a redirect, so it
    cannot reach any host but this one. The asset download stays anonymous.
    """
    request = urllib.request.Request(
        url, headers={'Accept': 'application/vnd.github+json', 'User-Agent': 'Prismattyc-update-fixture'})
    token = os.environ.get('GH_TOKEN') or os.environ.get('GITHUB_TOKEN')
    if token:
        assert '\n' not in token and '\r' not in token
        request.add_unredirected_header('Authorization', f'Bearer {token}')
    return request


def old_release(raw, version, target):
    """The checked release metadata and the pmux asset entry for `target`."""
    tag = 'v' + version
    assert len(raw) <= 4 * 1024 * 1024
    metadata = json.loads(raw)
    assert metadata['tag_name'] == tag and metadata['immutable'] and not metadata['draft'] and not metadata['prerelease']
    suffix = '.exe' if 'windows' in target else ''
    name = f'prismattyc-{tag}-{target}-pmux{suffix}'
    selected = [item for item in metadata['assets'] if item['name'] == name]
    assert len(selected) == 1
    asset = selected[0]
    expected_url = f'https://github.com/{REPOSITORY}/releases/download/{tag}/{name}'
    assert asset['browser_download_url'] == expected_url and 0 < asset['size'] <= 256 * 1024 * 1024
    assert re.fullmatch(r'sha256:[a-f0-9]{64}', asset['digest'])
    return asset, suffix


def verified(path, asset):
    return path.is_file() and path.stat().st_size == asset['size'] and 'sha256:' + digest(path) == asset['digest']


def fetch_old(version, target, directory, cache=None):
    """Fetch and verify an immutable older release's pmux into `directory`.

    With `cache`, a release fetched earlier is reused from there: its saved
    metadata and pmux are checked again and no request is made. A release is
    immutable, so a cache keyed on its version never goes stale.
    """
    assert re.fullmatch(r'0\.\d+\.\d+', version), 'Use a stable older release version'
    tag = 'v' + version
    cached = cache / tag if cache else None
    path = None
    if cached and (cached / 'old-release.json').is_file():
        raw = (cached / 'old-release.json').read_bytes()
        asset, suffix = old_release(raw, version, target)
        if verified(cached / asset['name'], asset):
            path = directory / ('verified-old-pmux' + suffix)
            shutil.copyfile(cached / asset['name'], path)
            print(f'using cached {tag} {asset["name"]}', file=sys.stderr)
    if path is None:
        api = f'https://api.github.com/repos/{REPOSITORY}/releases/tags/{tag}'

        def read_metadata():
            with urllib.request.urlopen(api_request(api), timeout=60) as response:
                return response.read(4 * 1024 * 1024 + 1)

        raw = with_retry(api, read_metadata)
        asset, suffix = old_release(raw, version, target)
        path = directory / ('verified-old-pmux' + suffix)

        def download():
            # Each attempt starts from nothing and leaves nothing behind.
            path.unlink(missing_ok=True)
            try:
                with urllib.request.urlopen(asset['browser_download_url'], timeout=120) as response, \
                        path.open('xb') as output:
                    assert urllib.parse.urlsplit(response.geturl()).scheme == 'https'
                    remaining = asset['size'] + 1
                    while remaining:
                        data = response.read(min(remaining, 1024 * 1024))
                        if not data:
                            break
                        output.write(data)
                        remaining -= len(data)
                if path.stat().st_size < asset['size']:
                    raise ConnectionError(f'download ended at {path.stat().st_size} of {asset["size"]} bytes')
            except BaseException:
                path.unlink(missing_ok=True)
                raise

        with_retry(asset['browser_download_url'], download)
        assert verified(path, asset)
        if cached:
            cached.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(path, cached / asset['name'])
            (cached / 'old-release.json').write_bytes(raw)
    assert verified(path, asset)
    path.chmod(0o700)
    (directory / 'old-release.json').write_bytes(raw)
    return path.resolve()


def digest(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


@contextmanager
def held_lock(path):
    with path.open('r+b') as file:
        if os.name == 'nt':
            import msvcrt
            msvcrt.locking(file.fileno(), msvcrt.LK_NBLCK, 1)
        else:
            import fcntl
            fcntl.flock(file, fcntl.LOCK_EX | fcntl.LOCK_NB)
        try:
            yield
        finally:
            if os.name == 'nt':
                file.seek(0)
                msvcrt.locking(file.fileno(), msvcrt.LK_UNLCK, 1)
            else:
                fcntl.flock(file, fcntl.LOCK_UN)


@contextmanager
def deny_windows_delete(path):
    import ctypes as C
    library = C.WinDLL('kernel32', use_last_error=True)
    library.CreateFileW.argtypes = [C.c_wchar_p, C.c_uint32, C.c_uint32, C.c_void_p, C.c_uint32, C.c_uint32, C.c_void_p]
    library.CreateFileW.restype = C.c_void_p
    library.CloseHandle.argtypes = [C.c_void_p]
    library.CloseHandle.restype = C.c_int
    handle = library.CreateFileW(str(path), 0x80000000, 1, None, 3, 0x80, None)
    assert handle != C.c_void_p(-1).value, C.get_last_error()
    try:
        yield
    finally:
        assert library.CloseHandle(handle)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bins', type=Path, required=True)
    old = parser.add_mutually_exclusive_group(required=True)
    old.add_argument('--old-pmux', type=Path, help='older official executable, verified before this drive')
    old.add_argument('--fetch-old-version', help='fetch and verify an immutable official older release before the drive')
    parser.add_argument('--target', choices=['x86_64-unknown-linux-gnu', 'aarch64-unknown-linux-gnu', 'x86_64-pc-windows-msvc', 'x86_64-pc-windows-gnu'])
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--cache-dir', type=Path, help='keep the fetched older release here and reuse it on later runs')
    args = parser.parse_args()
    if sys.platform not in ('linux', 'win32'):
        parser.error('requires native Linux or Windows; macOS uses verified app replacement')
    args.bins = args.bins.resolve()
    args.out.mkdir(parents=True, exist_ok=False)
    target = args.target or ('x86_64-pc-windows-msvc' if os.name == 'nt' else {'x86_64': 'x86_64-unknown-linux-gnu', 'aarch64': 'aarch64-unknown-linux-gnu'}[platform.machine()])
    args.old_pmux = fetch_old(args.fetch_old_version, target, args.out, args.cache_dir) if args.fetch_old_version else args.old_pmux.resolve()
    suffix = '.exe' if os.name == 'nt' else ''
    commands, checks, blocked = [], [], []
    result = {'status': 'FAIL', 'target': target, 'transport': 'local curl fixture; actual old and new native executables'}
    try:
        with tempfile.TemporaryDirectory(prefix='monitor-cutover-') as temporary:
            root = Path(temporary).resolve()
            prefix, tools, assets, releases = [root / name for name in ('installation', 'tools', 'assets', 'releases')]
            bins = prefix / 'bin'
            for path in (bins, tools, assets, releases):
                path.mkdir(parents=True)
            marker = root / 'old-collector-executed'
            env = {k: v for k, v in os.environ.items() if not k.startswith(('PMUX', 'PRISMATTYC_', 'XDG_'))}
            env.update(HOME=str(root / 'home'), USERPROFILE=str(root / 'home'), APPDATA=str(root / 'config'), LOCALAPPDATA=str(root / 'data'),
                XDG_DATA_HOME=str(root / 'data'), XDG_CONFIG_HOME=str(root / 'config'), XDG_STATE_HOME=str(root / 'state'), XDG_RUNTIME_DIR=str(root / 'runtime'),
                PMUX_SOCKET=str(root / 'runtime/no-daemon.sock'), PATH=str(tools) + os.pathsep + os.environ['PATH'],
                MONITOR_PROOF_ROOT=str(root), MONITOR_PROOF_PYTHON=sys.executable, MONITOR_PROOF_MARKER=str(marker))
            new_identity = json.loads(subprocess.check_output([str(args.bins / ('pmux' + suffix)), '--plugin-bundle'], env=env, text=True, encoding="utf-8"))
            assert new_identity['format'] == 'pmux-contained-plugin-consumer-v1'
            assert {p['id'] for p in new_identity['packages']} == {'org.moonbase.activity-monitor', 'org.moonbase.activity-connector'}
            assert new_identity['pin_sha256'] and new_identity['receipt_sha256']
            assert json.loads(subprocess.check_output([str(args.bins / (MONITOR + suffix)), '--plugin-bundle'], env=env, text=True, encoding="utf-8")) == new_identity
            new_version = new_identity['core_version']
            old_label = subprocess.check_output([str(args.old_pmux), '--version'], env=env, text=True, encoding="utf-8")
            old_version = re.search(r'\b\d+\.\d+\.\d+\b', old_label).group()
            assert tuple(map(int, old_version.split('.'))) < tuple(map(int, new_version.split('.')))
            old_help = subprocess.check_output([str(args.old_pmux), 'update', '--help'], env=env, text=True, encoding="utf-8")
            assert '--repair-monitor-launcher' not in old_help, 'The first process must actually predate auxiliary repair'
            env['MONITOR_PROOF_OLD_VERSION'] = old_version
            helper_source = root / 'fixture.rs'
            helper_source.write_text(r'''use std::{env,fs,path::Path,process::{Command,exit}};
fn main(){
 let name=env::current_exe().unwrap();
 let curl=Path::new(&name).file_stem().unwrap().to_string_lossy().to_ascii_lowercase()=="curl";
 if curl {
  let status=Command::new(env::var_os("MONITOR_PROOF_PYTHON").unwrap())
   .arg(Path::new(&env::var_os("MONITOR_PROOF_ROOT").unwrap()).join("curl.py"))
   .args(env::args_os().skip(1)).status().unwrap();exit(status.code().unwrap_or(99));
 }
 fs::write(env::var_os("MONITOR_PROOF_MARKER").unwrap(),b"executed").unwrap();
 println!("pmux-activity-monitor {}",env::var("MONITOR_PROOF_OLD_VERSION").unwrap());
}
''')
            helper = root / ('transport-helper' + suffix)
            subprocess.run(['rustc', '+1.90.0', '--edition=2021', '-O', str(helper_source), '-o', str(helper)], check=True)
            shutil.copy2(helper, tools / ('curl' + suffix))
            if os.name == 'nt':
                # Rust searches the executable's directory before System32,
                # then the inherited PATH. Scope the transport to our fixture.
                shutil.copy2(helper, bins / 'curl.exe')
            (root / 'curl.py').write_text('''import json,os,pathlib,sys
args=sys.argv[1:];root=pathlib.Path(os.environ['MONITOR_PROOF_ROOT']);url=args[-1]
with (root/'requests.jsonl').open('a') as output:output.write(json.dumps(url)+'\\n')
api='https://api.github.com/repos/moonbase2090/Prismattyc/releases/'
base='https://github.com/moonbase2090/Prismattyc/releases/download/'
if '--output' in args:
 if not url.startswith(base):sys.exit(22)
 source=root/'assets'/url.rsplit('/',1)[-1]
 pathlib.Path(args[args.index('--output')+1]).write_bytes(source.read_bytes())
elif url==api+'latest':print((root/'latest.json').read_text())
elif url.startswith(api+'tags/v'):
 print((root/'releases'/(url.rsplit('/',1)[-1]+'.json')).read_text())
else:sys.exit(22)
''')

            def asset(version, name, source):
                filename = f'prismattyc-v{version}-{target}-{name}{suffix}'
                path = assets / filename
                shutil.copy2(source, path)
                return {'name': filename, 'size': path.stat().st_size, 'digest': 'sha256:' + digest(path),
                    'browser_download_url': f'https://github.com/{REPOSITORY}/releases/download/v{version}/{filename}'}

            def release(version, members):
                value = {'tag_name': 'v' + version, 'draft': False, 'prerelease': False, 'immutable': True, 'assets': members}
                (releases / ('v' + version + '.json')).write_text(json.dumps(value))
                return value

            old_asset = asset(old_version, MONITOR, helper)
            release(old_version, [old_asset])
            current_release = release(new_version, [asset(new_version, name, args.bins / (name + suffix)) for name in (*CORE, MONITOR)])
            monitor_asset = next(a for a in current_release['assets'] if a['name'].endswith('-' + MONITOR + suffix))
            real_monitor = assets / monitor_asset['name']
            original_monitor = root / ('new-monitor-original' + suffix)
            shutil.copy2(real_monitor, original_monitor)
            (root / 'latest.json').write_text(json.dumps(current_release))
            for name in CORE:
                shutil.copy2(args.old_pmux, bins / (name + suffix))
            destination = bins / (MONITOR + suffix)
            shutil.copy2(helper, destination)
            old_hash, new_hash = digest(destination), digest(original_monitor)
            store = root / 'data/prismattyc/updates'
            auxiliary = store / 'monitor-launcher'
            executable = bins / ('pmux' + suffix)

            def cli(*arguments, success=True, program=executable):
                process = subprocess.run([str(program), 'update', *arguments], env=env, text=True, encoding="utf-8", capture_output=True, timeout=120)
                commands.append({'args': arguments, 'exit': process.returncode, 'stdout': process.stdout, 'stderr': process.stderr,
                                 'transport_requests': requests()})
                assert (process.returncode == 0) == success, commands[-1]
                return process

            def requests():
                path = root / 'requests.jsonl'
                return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

            def core_state():
                if os.name == 'nt':
                    pointer = (store / 'windows-current.json').read_bytes()
                    active = store / json.loads(pointer)['current']
                else:
                    pointer = [os.readlink(store / name) if (store / name).exists() else None for name in ('current', 'previous')]
                    active = store / 'current'
                return {'pointer': pointer, 'receipt': (active / 'receipt.json').read_bytes(), 'binaries': {name: digest(active / (name + suffix)) for name in CORE}}

            cli('--bin-dir', str(bins), program=executable)
            expected_downloads = {item['browser_download_url'] for item in current_release['assets']
                                  if item['name'] != monitor_asset['name']}
            assert set(requests()) == expected_downloads | {f'https://api.github.com/repos/{REPOSITORY}/releases/latest'}, commands[-1]
            assert digest(destination) == old_hash and not auxiliary.exists()
            state_before = core_state()
            expected_binaries = {name: digest(args.bins / (name + suffix)) for name in CORE}
            assert state_before['binaries'] == expected_binaries, {'actual':state_before['binaries'], 'expected':expected_binaries}
            if os.name == 'nt':
                active = store / json.loads((store / 'windows-current.json').read_bytes())['current']
                shutil.copy2(helper, active / 'curl.exe')
            checks.append('actual older process updated exactly six core executables and left the legacy monitor unchanged')
            future = '.'.join([*new_version.split('.')[:2], str(int(new_version.split('.')[2]) + 10)])
            (root / 'latest.json').write_text(json.dumps(release(future, [])))
            normal = ('--repair-monitor-launcher', '--from-monitor-version', old_version, '--json')

            def untouched():
                assert digest(destination) == old_hash and core_state() == state_before
                assert not marker.exists(), 'An old unrestricted collector was executed during verification'

            before_requests = len(requests())
            cli('--repair-monitor-launcher', '--json', success=False)
            assert len(requests()) == before_requests
            untouched()
            checked = cli(*normal, '--check')
            assert json.loads(checked.stdout)['status'] == 'would_repair'
            untouched()
            assert not auxiliary.exists(), 'Check created a receipt or backup'
            assert not any(url.endswith('/latest') for url in requests()[before_requests:])
            checks.append('check verified the exact installed release without receipt, backup, launcher, or core mutation')
            other = root / 'other-bin'
            other.mkdir()
            cli(*normal, '--bin-dir', str(other), success=False)
            untouched()
            cli('--repair-monitor-launcher', '--from-monitor-version', new_version, success=False)
            untouched()
            preserved = root / ('legacy-preserved' + suffix)
            shutil.copy2(destination, preserved)
            destination.write_bytes(destination.read_bytes() + b'foreign bytes claiming the same version')
            foreign = digest(destination)
            cli(*normal, success=False)
            assert digest(destination) == foreign and not marker.exists()
            shutil.copy2(preserved, destination)
            destination.unlink()
            try:
                destination.symlink_to(preserved)
            except OSError as error:
                blocked.append('file symlink/reparse prerequisite: ' + str(error))
            else:
                cli(*normal, success=False)
                assert destination.is_symlink() and digest(preserved) == old_hash
                destination.unlink()
            shutil.copy2(preserved, destination)
            untouched()
            checks.append('unknown bytes, wrong source hint, other installation, and available symlink cases refused')

            raw = real_monitor.read_bytes()
            real_monitor.write_bytes(raw[:-1] + bytes([raw[-1] ^ 1]))
            cli(*normal, success=False)
            untouched()
            real_monitor.write_bytes(raw)
            for mutation in ('mutable', 'duplicate', 'missing digest', 'wrong origin'):
                changed = json.loads(json.dumps(current_release))
                item = next(a for a in changed['assets'] if a['name'] == monitor_asset['name'])
                if mutation == 'mutable': changed['immutable'] = False
                elif mutation == 'duplicate': changed['assets'].append(dict(item))
                elif mutation == 'missing digest': item.pop('digest')
                else: item['browser_download_url'] = 'https://example.invalid/monitor'
                (releases / ('v' + new_version + '.json')).write_text(json.dumps(changed))
                cli(*normal, success=False)
                untouched()
                assert 'https://example.invalid/monitor' not in requests()
            (releases / ('v' + new_version + '.json')).write_text(json.dumps(current_release))
            for source, label in ((helper, 'old collector'), (args.bins / ('pmux-mcp' + suffix), 'wrong executable role')):
                shutil.copy2(source, real_monitor)
                changed = json.loads(json.dumps(current_release))
                item = next(a for a in changed['assets'] if a['name'] == monitor_asset['name'])
                item.update(size=real_monitor.stat().st_size, digest='sha256:' + digest(real_monitor))
                (releases / ('v' + new_version + '.json')).write_text(json.dumps(changed))
                cli(*normal, success=False)
                untouched()
                checks.append(label + ' rejected before replacement; old collector never probed')
            shutil.copy2(original_monitor, real_monitor)
            (releases / ('v' + new_version + '.json')).write_text(json.dumps(current_release))
            for path in (store / 'update.lock', prefix / '.prismattyc-install.lock'):
                with held_lock(path):
                    cli(*normal, success=False)
                untouched()
            checks.append('both core-update and installation locks prevent mutation')
            if os.name == 'nt':
                with deny_windows_delete(destination):
                    cli(*normal, success=False)
                untouched()
                pending = json.loads((auxiliary / 'receipt.json').read_text())
                assert pending['installed']['sha256'] == new_hash and pending['previous']['fingerprint']['sha256'] == old_hash
                checks.append('Windows sharing failure preserved old launcher and a recoverable prepared receipt')
                repaired = cli('--repair-monitor-launcher', '--json')
            else:
                repaired = cli(*normal)
            assert json.loads(repaired.stdout)['status'] == 'repaired'
            assert digest(destination) == new_hash and core_state() == state_before and not marker.exists()
            saved = json.loads((auxiliary / 'receipt.json').read_text())
            backup = auxiliary / ('backup-' + old_hash)
            assert digest(backup) == old_hash and saved['installed']['sha256'] == new_hash
            assert saved['previous']['fingerprint']['sha256'] == old_hash
            receipt_before = (auxiliary / 'receipt.json').read_bytes()
            replayed = cli('--repair-monitor-launcher', '--json')
            assert json.loads(replayed.stdout)['status'] == 'current'
            assert (auxiliary / 'receipt.json').read_bytes() == receipt_before
            assert digest(destination) == new_hash and core_state() == state_before
            checks.append('same-version second-process repair installs only the matched frontend; replay is idempotent')
            # Seed the recorded pre-rename state from the operation's actual journal and backup.
            shutil.copy2(backup, destination)
            cli('--repair-monitor-launcher', '--json')
            assert digest(destination) == new_hash and core_state() == state_before and not marker.exists()
            checks.append('recorded old bytes plus prepared receipt resume to the same installed frontend')
            destination.write_bytes(destination.read_bytes() + b'foreign mutation')
            changed_hash = digest(destination)
            before = (auxiliary / 'receipt.json').read_bytes()
            cli('--repair-monitor-launcher', '--json', success=False)
            assert digest(destination) == changed_hash and (auxiliary / 'receipt.json').read_bytes() == before
            shutil.copy2(original_monitor, destination)
            destination.unlink()
            cli('--repair-monitor-launcher', '--json')
            assert digest(destination) == new_hash and core_state() == state_before
            before_rollback = {p.name: digest(p) for p in auxiliary.iterdir() if p.is_file()}
            cli('--rollback')
            assert digest(destination) == new_hash
            assert {p.name: digest(p) for p in auxiliary.iterdir() if p.is_file()} == before_rollback
            active = store / (json.loads((store / 'windows-current.json').read_text())['current'] if os.name == 'nt' else 'current')
            assert all(digest(active / (name + suffix)) == digest(args.old_pmux) for name in CORE)
            assert not (active / (MONITOR + suffix)).exists()
            assert not marker.exists() and not (root / 'runtime/no-daemon.sock').exists()
            checks.append('core rollback restores six old members and preserves auxiliary frontend, history, backup, and receipt')
            result.update(status='PARTIAL' if blocked else 'PASS', old_version=old_version, new_version=new_version,
                old_core_sha256=digest(args.old_pmux), new_monitor_sha256=new_hash, checks=checks, blocked=blocked,
                prepared_state='seeded from the actual journal and verified backup; no SIGKILL timing claim',
                windows_sharing='exercised' if os.name == 'nt' else 'requires native Windows')
    finally:
        (args.out / 'commands.json').write_text(json.dumps(commands, indent=2) + '\n')
        (args.out / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps(result, indent=2))
    return 2 if blocked else 0


if __name__ == '__main__':
    raise SystemExit(main())
