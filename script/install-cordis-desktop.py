#!/usr/bin/env python3
"""Install a macOS Electron ZIP with transparent filesystem compression."""

import argparse
import ctypes
import os
import subprocess
import sys
import tempfile
from pathlib import Path


def publish(source, destination):
    # Darwin can publish a directory atomically and refuse any existing destination.
    rename = ctypes.CDLL(None, use_errno=True).renamex_np
    rename.argtypes = (ctypes.c_char_p, ctypes.c_char_p, ctypes.c_uint)
    rename.restype = ctypes.c_int
    rename_excl = 0x00000004
    if rename(os.fsencode(source), os.fsencode(destination), rename_excl) != 0:
        error = ctypes.get_errno()
        raise OSError(error, os.strerror(error), destination)


def install(archive, destination):
    if not archive.is_file():
        raise FileNotFoundError(f"Electron ZIP does not exist: {archive}")
    if os.path.lexists(destination):
        raise FileExistsError(f"Installation destination already exists: {destination}")
    destination.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".cordis-install-", dir=destination.parent) as work:
        unpacked = Path(work) / "unpacked"
        staged = Path(work) / "compressed"
        subprocess.run(["/usr/bin/ditto", "-x", "-k", archive, unpacked], check=True)
        contents = unpacked / "Electron.app/Contents"
        framework = contents / "Frameworks/Electron Framework.framework/Versions/A"
        executable = contents / "MacOS/Electron"
        for required in (unpacked / "version", contents / "Info.plist", executable,
                         framework / "Electron Framework", framework / "Resources/icudtl.dat",
                         framework / "Resources/resources.pak"):
            if not required.is_file() or required.stat().st_size == 0:
                raise ValueError(f"Incomplete macOS Electron ZIP, missing {required.relative_to(unpacked)}")
        if not os.access(executable, os.X_OK):
            raise ValueError("Electron.app/Contents/MacOS/Electron is not executable")
        # The file copy applies compression while preserving bundle bytes and metadata.
        # APFS cloning would leave the files in their original compression state.
        subprocess.run(["/usr/bin/ditto", "--nocache", "--noclone", "--hfsCompression", unpacked, staged],
                       check=True)
        publish(staged, destination)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("archive", type=Path)
    parser.add_argument("destination", type=Path)
    args = parser.parse_args()
    if sys.platform != "darwin":
        parser.error("Desktop installation requires macOS")
    invocation_dir = Path(os.environ.get("CORDIS_INSTALL_CWD") or Path.cwd())
    archive = invocation_dir / args.archive.expanduser()
    destination = invocation_dir / args.destination.expanduser()
    try:
        install(archive, destination)
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        print(f"[install-desktop] {error}", file=sys.stderr)
        return 1
    print(destination / "Electron.app")
    return 0


if __name__ == "__main__":
    sys.exit(main())
