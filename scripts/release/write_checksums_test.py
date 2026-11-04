#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Check the release checksum command against real files and literal digests."""

from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).with_name('write_checksums.py')
EXPECTED = (
    'ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  a.bin\n'
    'e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  archive with spaces.zip\n'
)


class ReleaseChecksumTests(unittest.TestCase):
    def run_checksum_command(self, directory):
        subprocess.run([sys.executable, str(SCRIPT), str(directory)], check=True)

    def test_lists_only_real_assets_with_their_digests(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            directory = root / 'dist'
            directory.mkdir()
            (directory / 'a.bin').write_bytes(b'abc')
            (directory / 'archive with spaces.zip').write_bytes(b'')
            self.run_checksum_command(directory)
            self.assertEqual((directory / 'SHA256SUMS').read_text(), EXPECTED)
            self.assertEqual(
                {path.name for path in directory.iterdir()},
                {'a.bin', 'archive with spaces.zip', 'SHA256SUMS'},
            )
            self.assertEqual({path.name for path in root.iterdir()}, {'dist'})

    def test_replacing_an_old_list_and_repeating_does_not_hash_the_output(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary) / 'dist'
            directory.mkdir()
            (directory / 'a.bin').write_bytes(b'abc')
            (directory / 'archive with spaces.zip').write_bytes(b'')
            (directory / 'SHA256SUMS').write_text('obsolete checksum list\n')
            self.run_checksum_command(directory)
            self.run_checksum_command(directory)
            self.assertEqual((directory / 'SHA256SUMS').read_text(), EXPECTED)


if __name__ == '__main__':
    unittest.main()
