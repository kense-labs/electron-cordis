#!/usr/bin/env python3
"""Check an electron-builder distribution without unpacking its runtime."""

import argparse
import plistlib
import struct
import sys
from pathlib import Path
from xml.parsers.expat import ExpatError
from zipfile import BadZipFile, ZipFile

MAC_CONTENTS = "Electron.app/Contents"
MAC_FRAMEWORK = f"{MAC_CONTENTS}/Frameworks/Electron Framework.framework/Versions/A"
MAC_RENDERER = f"{MAC_CONTENTS}/Frameworks/Electron Helper (Renderer).app/Contents"
MACH_CPUS = {"arm64": 0x0100000C, "x64": 0x01000007}
PE_MACHINES = {"arm64": 0xAA64, "x64": 0x8664}


def check_mach_cpu(binary, arch):
    header = binary.read(8)
    if len(header) != 8:
        return False
    magic = header[:4]
    if magic == b"\xcf\xfa\xed\xfe":
        return int.from_bytes(header[4:], "little") == MACH_CPUS[arch]
    if magic == b"\xfe\xed\xfa\xcf":
        return int.from_bytes(header[4:], "big") == MACH_CPUS[arch]

    # A universal binary has an architecture table before its Mach-O slices.
    fat_headers = {b"\xca\xfe\xba\xbe": 20, b"\xca\xfe\xba\xbf": 32}
    entry_size = fat_headers.get(magic)
    count = int.from_bytes(header[4:], "big")
    if not entry_size or not 0 < count <= 32:
        return False
    entries = binary.read(count * entry_size)
    return len(entries) == count * entry_size and any(
        struct.unpack_from(">I", entries, offset)[0] == MACH_CPUS[arch]
        for offset in range(0, len(entries), entry_size)
    )


def check_pe_machine(binary, arch):
    header = binary.read(64)
    if len(header) != 64 or header[:2] != b"MZ":
        return False
    offset, = struct.unpack_from("<I", header, 0x3C)
    # Avoid decompressing large parts of a ZIP to follow an invalid header.
    if offset < len(header) or offset > 1024 * 1024:
        return False
    binary.seek(offset)
    return binary.read(6) == b"PE\0\0" + struct.pack("<H", PE_MACHINES[arch])


def read_metadata(open_file, name, limit):
    with open_file(name) as entry:
        data = entry.read(limit + 1)
    if len(data) > limit:
        raise ValueError(f"Metadata is too large: {name}")
    return data


def check_version(actual, expected, source):
    if actual != expected:
        raise ValueError(f"{source} reports Electron {actual!r}, expected {expected}")


def check_contents(open_file, version, platform, arch):
    try:
        recorded_version = read_metadata(open_file, "version", 256).decode("ascii").strip()
    except (FileNotFoundError, KeyError):
        if platform == "win32":
            raise ValueError("Missing version file") from None
    else:
        check_version(recorded_version, version, "version")

    if platform == "darwin":
        name = f"{MAC_CONTENTS}/Info.plist"
        info = plistlib.loads(read_metadata(open_file, name, 64 * 1024))
        if not isinstance(info, dict):
            raise ValueError(f"Expected a property dictionary in {name}")
        check_version(info.get("CFBundleVersion"), version, name)
        binaries = [
            f"{MAC_CONTENTS}/MacOS/Electron",
            f"{MAC_FRAMEWORK}/Electron Framework",
            f"{MAC_RENDERER}/MacOS/Electron Helper (Renderer)",
        ]
        resource_root = f"{MAC_FRAMEWORK}/Resources/"
        check_arch = check_mach_cpu
    else:
        binaries = ["electron.exe", "ffmpeg.dll"]
        resource_root = ""
        check_arch = check_pe_machine

    for name in binaries:
        with open_file(name) as binary:
            if not check_arch(binary, arch):
                raise ValueError(f"{name} does not contain a {platform}-{arch} executable")
    for filename in ("icudtl.dat", "resources.pak"):
        name = resource_root + filename
        with open_file(name) as resource:
            if not resource.read(1):
                raise ValueError(f"Empty resource: {name}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("distribution", type=Path)
    parser.add_argument("version")
    parser.add_argument("platform", choices=("darwin", "win32"))
    parser.add_argument("arch", choices=MACH_CPUS)
    args = parser.parse_args()
    dist = args.distribution
    try:
        if dist.suffix != ".zip" and dist.is_dir():
            cached_zip = dist / f"electron-v{args.version}-{args.platform}-{args.arch}.zip"
            if cached_zip.exists():
                dist = cached_zip
        if dist.suffix == ".zip":
            with ZipFile(dist) as archive:
                check_contents(archive.open, args.version, args.platform, args.arch)
        elif dist.is_dir():
            check_contents(lambda entry: (dist / entry).open("rb"), args.version, args.platform, args.arch)
        else:
            raise ValueError("Expected a distribution ZIP or directory")
    except (OSError, ValueError, KeyError, BadZipFile, ExpatError) as error:
        print(f"Invalid ELECTRON_CORDIS_DIST {dist}: {error}", file=sys.stderr)
        return 1
    print(dist)
    return 0


if __name__ == "__main__":
    sys.exit(main())
