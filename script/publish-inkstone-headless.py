"""Publish a staged Headless Cordis distribution on the destination filesystem."""

import argparse
import ctypes
import errno
import fcntl
import hashlib
import json
import os
import sys
import tempfile
from pathlib import Path


def check_distribution(directory: Path, *, allow_empty: bool = False) -> None:
    if directory.is_symlink() or not directory.is_dir():
        raise ValueError(f"Not a headless distribution directory: {directory}")
    entries = list(directory.rglob("*"))
    if allow_empty and not entries:
        return
    if any(entry.is_symlink() or not (entry.is_file() or entry.is_dir()) for entry in entries):
        raise ValueError(f"Unexpected filesystem entries in {directory}")

    manifest_path = directory / "MANIFEST.json"
    if not manifest_path.is_file():
        raise ValueError(f"Missing headless distribution manifest: {manifest_path}")
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    if manifest.get("schema") != "electron-cordis.headless-dist.v1" or manifest.get("profile") != "inkstone-headless":
        raise ValueError(f"Unrecognized headless distribution manifest: {manifest_path}")

    files = manifest["files"]
    expected = {Path("MANIFEST.json")}
    for name in files:
        path = Path(name)
        expected.add(path)
        expected.update(parent for parent in path.parents if parent != Path("."))
    if {entry.relative_to(directory) for entry in entries} != expected:
        raise ValueError(f"Distribution contains unexpected or missing files: {directory}")
    for name, metadata in files.items():
        data = (directory / name).read_bytes()
        if len(data) != metadata["bytes"] or hashlib.sha256(data).hexdigest() != metadata["sha256"]:
            raise ValueError(f"Distribution file differs from its manifest: {directory / name}")


def exchange_on_macos(staged: Path, destination: Path) -> bool:
    if sys.platform != "darwin":
        return False
    exchange = ctypes.CDLL(None, use_errno=True).renamex_np
    exchange.argtypes = (ctypes.c_char_p, ctypes.c_char_p, ctypes.c_uint)
    exchange.restype = ctypes.c_int
    rename_swap = 0x00000002  # RENAME_SWAP from the macOS SDK.
    if exchange(os.fsencode(staged), os.fsencode(destination), rename_swap) == 0:
        return True
    error = ctypes.get_errno()
    if error in (errno.ENOTSUP, errno.EOPNOTSUPP):
        return False
    raise OSError(error, os.strerror(error), destination)


def recover(destination: Path, previous: Path, work: Path) -> None:
    if not os.path.lexists(previous):
        return
    check_distribution(previous, allow_empty=True)
    if not os.path.lexists(destination):
        previous.rename(destination)
        return
    check_distribution(destination, allow_empty=True)
    retired = Path(tempfile.mkdtemp(prefix="retired-", dir=work))
    previous.rename(retired / "package")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("destination", type=Path)
    parser.add_argument("work", type=Path)
    parser.add_argument("--staged", type=Path)
    args = parser.parse_args()
    destination = args.destination
    previous = destination.with_name(f".{destination.name}.cordis-previous")

    descriptor = os.open(destination.parent, os.O_RDONLY | os.O_DIRECTORY)
    try:
        fcntl.flock(descriptor, fcntl.LOCK_EX)
        recover(destination, previous, args.work)
        if os.path.lexists(destination):
            check_distribution(destination, allow_empty=True)
        if args.staged is None:
            return

        check_distribution(args.staged)
        if not destination.exists() or not any(destination.iterdir()):
            args.staged.replace(destination)
        elif not exchange_on_macos(args.staged, destination):
            destination.rename(previous)
            try:
                args.staged.rename(destination)
            except BaseException:
                previous.rename(destination)
                raise
            recover(destination, previous, args.work)
        print(f"[pack-inkstone-headless] Installed verified distribution at {destination}")
    finally:
        os.close(descriptor)


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, TypeError) as error:
        sys.exit(f"[pack-inkstone-headless] {error}")
