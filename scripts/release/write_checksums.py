#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Atomically write SHA256SUMS for a flat release asset directory."""

import argparse
import hashlib
import os
from pathlib import Path
import tempfile


def write_checksums(directory: Path) -> None:
    assets = sorted(
        path for path in directory.iterdir()
        if path.is_file() and path.name != 'SHA256SUMS'
    )
    # Keep incomplete output outside the directory that contains public assets.
    with tempfile.TemporaryDirectory(prefix='.release-checksums-', dir=directory.parent) as temporary:
        output = Path(temporary) / 'SHA256SUMS'
        with output.open('w', encoding='utf-8', newline='\n') as stream:
            for asset in assets:
                with asset.open('rb') as source:
                    digest = hashlib.file_digest(source, 'sha256').hexdigest()
                stream.write(f'{digest}  {asset.name}\n')
        os.replace(output, directory / 'SHA256SUMS')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('directory', type=Path)
    write_checksums(parser.parse_args().directory)
