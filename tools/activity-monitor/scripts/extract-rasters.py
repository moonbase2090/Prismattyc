#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Save the Kitty graphics rasters in an asciicast as PNG files, unchanged.

Reads the monitor's recorded output, reassembles each `a=t` raw RGBA (f=32) upload, and
writes it as OUT/raster-NNN-IDxGEN.png at its own pixel size. Nothing is composited.

  extract-rasters.py demo.cast OUT [--last N]
"""

import argparse
import base64
import json
import re
import struct
import sys
import zlib
from pathlib import Path

APC = re.compile(r'\x1b_G([^;\x1b]*);?([^\x1b]*)\x1b\\')


def png(width, height, rgba):
    def chunk(kind, data):
        body = kind + data
        return struct.pack('>I', len(data)) + body + struct.pack('>I', zlib.crc32(body) & 0xffffffff)
    stride = width * 4
    raw = b''.join(b'\x00' + rgba[y * stride:(y + 1) * stride] for y in range(height))
    return (b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', width, height, 8, 6, 0, 0, 0))
            + chunk(b'IDAT', zlib.compress(raw, 9)) + chunk(b'IEND', b''))


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('cast', type=Path)
    parser.add_argument('out', type=Path)
    parser.add_argument('--last', type=int, default=0, help='keep only the last N rasters')
    args = parser.parse_args()
    text = ''.join(json.loads(line)[2] for line in args.cast.read_text().splitlines()[1:]
                   if json.loads(line)[1] == 'o')
    rasters, pending, control = [], [], {}
    for match in APC.finditer(text):
        fields = dict(part.split('=', 1) for part in match.group(1).split(',') if '=' in part)
        if fields.get('a') == 't' or (pending and 'a' not in fields):
            if fields.get('a') == 't':
                control, pending = fields, []
            pending.append(match.group(2))
            if fields.get('m', '0') == '0':
                data = b''.join(base64.b64decode(piece) for piece in pending)
                if control.get('o') == 'z':
                    data = zlib.decompress(data)
                rasters.append((control, data))
                pending = []
    args.out.mkdir(parents=True, exist_ok=True)
    chosen = rasters[-args.last:] if args.last else rasters
    for index, (fields, data) in enumerate(chosen):
        if fields.get('f') != '32':
            continue
        width, height = int(fields['s']), int(fields['v'])
        name = f"raster-{index:03d}-{fields.get('i', 'x')}.png"
        (args.out / name).write_bytes(png(width, height, data))
    print(json.dumps({'uploads': len(rasters), 'written': len(chosen)}))


if __name__ == '__main__':
    sys.exit(main())
