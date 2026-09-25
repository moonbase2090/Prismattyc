#!/usr/bin/env python3
"""Copy the reviewed dual-format terminal database into a release resource tree."""
import argparse
import pathlib
import shutil

ROOT = pathlib.Path(__file__).resolve().parents[1]
ENTRIES = {
    'prismattyc-kitty': ('prismattyc-kitty', 'prismattyc-direct', 'prism-kitty', 'prism-direct'),
    'prismattyc-256color': ('prismattyc-256color', 'prism-256color', 'prism'),
    'prismattyc-16color': ('prismattyc-16color', 'prism-16color'),
}


def package(out):
    for name, aliases in ENTRIES.items():
        for layout, source in [('p', ROOT / 'terminfo/p'),
                               ('70', ROOT / 'terminfo/legacy/p')]:
            data = (source / name).read_bytes()
            if layout == '70' and (data[:2] != b'\x1a\x01' or len(data) > 4096):
                raise ValueError(f'{name} is not an Apple-compatible legacy entry')
            (out / layout).mkdir(parents=True, exist_ok=True)
            for alias in aliases:
                (out / layout / alias).write_bytes(data)
    shutil.copyfile(ROOT / 'terminfo/portable.src', out / 'portable.src')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--out', required=True, type=pathlib.Path)
    package(parser.parse_args().out)
