"""Read a Windows group-icon resource and the release ICO sizes.

The proof script passes packaged executables. The unit test builds a synthetic
PE so the parser fails when icon resource ID 1 is missing.
"""

import struct
import sys
from pathlib import Path

RT_GROUP_ICON = 14


def _u16(blob, offset):
    return struct.unpack_from("<H", blob, offset)[0]


def _u32(blob, offset):
    return struct.unpack_from("<I", blob, offset)[0]


def ico_sizes(data):
    """Directory widths from an .ico file. A zero dimension means 256."""
    if len(data) < 6 or data[:4] != b"\x00\x00\x01\x00":
        raise ValueError("not a Windows icon")
    count = _u16(data, 4)
    sizes = []
    for index in range(count):
        entry = 6 + index * 16
        if entry >= len(data):
            break
        width = data[entry]
        sizes.append(256 if width == 0 else width)
    return sizes


def contains_utf16(data, text):
    return text.encode("utf-16le") in data


def _sections(pe, opt, opt_size, count):
    sections = []
    start = opt + opt_size
    for index in range(count):
        header = start + index * 40
        virtual_size = _u32(pe, header + 8)
        virtual_address = _u32(pe, header + 12)
        raw_size = _u32(pe, header + 16)
        raw_pointer = _u32(pe, header + 20)
        sections.append((virtual_address, max(virtual_size, raw_size), raw_pointer))
    return sections


def _rva_to_offset(sections, rva):
    for virtual_address, size, raw in sections:
        if virtual_address <= rva < virtual_address + size:
            return raw + (rva - virtual_address)
    raise ValueError(f"RVA {rva:#x} is not in a section")


def _directory_entries(pe, offset):
    named = _u16(pe, offset + 12)
    ids = _u16(pe, offset + 14)
    entries = []
    base = offset + 16
    for index in range(named + ids):
        entry = base + index * 8
        entries.append((_u32(pe, entry), _u32(pe, entry + 4)))
    return entries


def icon_widths(pe):
    """GRPICONDIRENTRY widths for RT_GROUP_ICON resource ID 1.

    A missing resource or a different id returns an empty list. Width 0 is
    reported as 256.
    """
    if len(pe) < 0x40 or pe[:2] != b"MZ":
        raise ValueError("not a PE")
    lfanew = _u32(pe, 0x3C)
    if pe[lfanew : lfanew + 4] != b"PE\0\0":
        raise ValueError("missing PE signature")
    coff = lfanew + 4
    section_count = _u16(pe, coff + 2)
    optional_size = _u16(pe, coff + 16)
    optional = coff + 20
    magic = _u16(pe, optional)
    if magic == 0x20B:
        number_offset = 108
    elif magic == 0x10B:
        number_offset = 92
    else:
        raise ValueError(f"unknown optional header {magic:#x}")
    directory_count = _u32(pe, optional + number_offset)
    if directory_count <= 2:
        return []
    directories = optional + number_offset + 4
    resource_rva = _u32(pe, directories + 16)
    if resource_rva == 0:
        return []
    sections = _sections(pe, optional, optional_size, section_count)
    root = _rva_to_offset(sections, resource_rva)

    def subdirectory(offset, wanted):
        for name, data in _directory_entries(pe, offset):
            if name == wanted and data & 0x80000000:
                return root + (data & 0x7FFFFFFF)
        return None

    group = subdirectory(root, RT_GROUP_ICON)
    if group is None:
        return []
    named = subdirectory(group, 1)
    if named is None:
        return []
    languages = _directory_entries(pe, named)
    if not languages or languages[0][1] & 0x80000000:
        return []
    data_entry = root + languages[0][1]
    blob_offset = _rva_to_offset(sections, _u32(pe, data_entry))
    blob_size = _u32(pe, data_entry + 4)
    blob = pe[blob_offset : blob_offset + blob_size]
    if len(blob) < 6 or _u16(blob, 2) != 1:
        return []
    count = _u16(blob, 4)
    widths = []
    for index in range(count):
        entry = 6 + index * 14
        if entry >= len(blob):
            break
        width = blob[entry]
        widths.append(256 if width == 0 else width)
    return widths


def synthetic_pe(widths, icon_id=1):
    """A PE32+ whose only resource is a group icon of the given widths."""
    count = len(widths)
    group = bytearray(6 + 14 * count)
    struct.pack_into("<HHH", group, 0, 0, 1, count)
    for index, width in enumerate(widths):
        stored = 0 if width >= 256 else width
        struct.pack_into(
            "<BBBBHHIH", group, 6 + index * 14, stored, stored, 0, 0, 1, 32, 0, index + 1
        )

    # Type, id, and language directories, then one data entry, then the group.
    group_at = 88
    resource = bytearray(group_at + len(group))

    def directory(offset, name, child, subdirectory):
        struct.pack_into("<IIHHHH", resource, offset, 0, 0, 0, 0, 0, 1)
        flag = 0x80000000 if subdirectory else 0
        struct.pack_into("<II", resource, offset + 16, name, child | flag)

    directory(0, RT_GROUP_ICON, 24, True)
    directory(24, icon_id, 48, True)
    directory(48, 0, 72, False)
    struct.pack_into("<IIII", resource, 72, 0x1000 + group_at, len(group), 0, 0)
    resource[group_at:] = group

    optional_size = 240
    section_raw = 0x200
    pe = bytearray(section_raw + len(resource))
    pe[0:2] = b"MZ"
    struct.pack_into("<I", pe, 0x3C, 64)
    struct.pack_into("<IHHIIIHH", pe, 64, 0x00004550, 0x8664, 1, 0, 0, 0, optional_size, 0x22)
    optional = 88
    struct.pack_into("<H", pe, optional, 0x20B)
    struct.pack_into("<I", pe, optional + 108, 16)
    struct.pack_into("<II", pe, optional + 112 + 16, 0x1000, len(resource))
    header = optional + optional_size
    pe[header : header + 8] = b".rsrc\0\0\0"
    struct.pack_into("<IIII", pe, header + 8, len(resource), 0x1000, len(resource), section_raw)
    pe[section_raw : section_raw + len(resource)] = resource
    return bytes(pe)


def main(argv):
    required = {16, 24, 32, 48, 256}
    problems = []
    for raw in argv[1:]:
        path = Path(raw)
        data = path.read_bytes()
        widths = set(icon_widths(data))
        if not required.issubset(widths):
            problems.append(f"{path.name}: icon sizes {sorted(widths)}")
        for text in ("Prismattyc", "Moonbase 2090 LLC"):
            if not contains_utf16(data, text):
                problems.append(f"{path.name}: missing version string {text}")
    if problems:
        print("\n".join(problems), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
