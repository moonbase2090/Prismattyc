#!/usr/bin/env python3
"""Exercise packaged databases and explicit installer through ncurses consumers."""
import os
import pathlib
import shutil
import subprocess
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
NAMES = ('prismattyc-kitty', 'prismattyc-direct', 'prism-kitty', 'prism-direct',
         'prismattyc-256color', 'prism-256color', 'prism', 'prismattyc-16color', 'prism-16color')


def run(*args, **kwargs):
    return subprocess.run(args, capture_output=True, text=True, check=True, **kwargs)


class TerminfoTests(unittest.TestCase):
    def test_packaged_database_and_fresh_home_install(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            database = root / 'Prismattyc.app/Contents/Resources/terminfo'
            run('python3', str(ROOT / 'scripts/package-terminfo.py'), '--out', str(database))
            home = root / 'home'
            home.mkdir()
            env = dict(os.environ, HOME=str(home), TERMINFO=str(database), TERMINFO_DIRS=str(database))
            self.assertFalse((home / '.terminfo').exists())
            for name in NAMES:
                run('infocmp', '-x', name, env=env)
                self.assertTrue(run('tput', '-T', name, 'clear', env=env).stdout.startswith('\x1b['))
                # Validate the actual serialized legacy contract, not source strings.
                data = (database / '70' / name).read_bytes()
                self.assertEqual(data[:2], b'\x1a\x01')
                self.assertLessEqual(len(data), 4096)
            helper = root / 'Prismattyc.app/Contents/MacOS/install-prismattyc-terminfo.sh'
            helper.parent.mkdir()
            shutil.copyfile(ROOT / 'scripts/install-prismattyc-terminfo.sh', helper)
            # Force the packaged helper/source through the real compiler and HOME lookup.
            env.pop('TERMINFO')
            env.pop('TERMINFO_DIRS')
            run('sh', str(helper), env=env)
            for name in NAMES:
                run('infocmp', name, env=env)
                self.assertGreater(int(run('tput', '-T', name, 'colors', env=env).stdout), 0)
            # Repeated installation is safe.
            run('sh', str(helper), env=env)

    @unittest.skipUnless(os.environ.get('PRISMATTYC_TERMINFO_SSH_TEST') == '1', 'isolated SSH box required')
    def test_real_ssh_install(self):
        # Container fixture supplies a new home and a trusted localhost SSH alias.
        before = subprocess.run(['ssh', 'terminfo-test', 'infocmp prismattyc-kitty'], capture_output=True)
        self.assertNotEqual(before.returncode, 0, 'fixture must begin without installed terminfo')
        run('sh', str(ROOT / 'scripts/install-prismattyc-terminfo.sh'), '--ssh', 'terminfo-test')
        for name in NAMES:
            run('ssh', 'terminfo-test', f'TERM={name} tput clear')
            run('ssh', 'terminfo-test', f'TERM={name} clear')
        run('ssh', '-tt', 'terminfo-test', 'TERM=prismattyc-kitty less -F /etc/hostname', timeout=15)
        run('ssh', '-tt', 'terminfo-test', 'TERM=prismattyc-kitty vim -Nu NONE -n -c q', timeout=15)
        run('ssh', '-tt', 'terminfo-test', 'TERM=xterm-256color tput clear')


if __name__ == '__main__':
    unittest.main()
