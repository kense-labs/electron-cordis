#!/usr/bin/env python3
"""Package a prepared native Cordis plugin and sign its installation manifest."""

import argparse
import gzip
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import sys
import tarfile
from tempfile import TemporaryDirectory


SIGN_MANIFEST = """
const { createPrivateKey, sign } = require('node:crypto');
const { readFileSync } = require('node:fs');
const key = createPrivateKey(readFileSync(process.argv[1]));
if (key.asymmetricKeyType !== 'ed25519') throw new Error('Expected an Ed25519 private key');
process.stdout.write(sign(null, readFileSync(0), key).toString('hex'));
"""
PACKAGE_FILES = {'archive.tar.gz', 'manifest.json'}


def file_hash(path):
    digest = hashlib.sha256()
    with path.open('rb') as source:
        for block in iter(lambda: source.read(128 * 1024), b''):
            digest.update(block)
    return digest.hexdigest()


def file_identity(info):
    return info.st_dev, info.st_ino, info.st_size, info.st_mtime_ns


def fail_walk(error):
    raise error


def pack(source, destination, private_key):
    key_identity = private_key.stat()
    count = file_bytes = 0
    with destination.open('wb') as output, gzip.GzipFile(
        filename='', mode='wb', fileobj=output, mtime=0, compresslevel=6,
    ) as compressed:
        with tarfile.open(
            fileobj=compressed, mode='w|', format=tarfile.PAX_FORMAT, dereference=True,
        ) as archive:
            for parent, directories, files in os.walk(source, onerror=fail_walk):
                directories.sort()
                for name in sorted(directories + files):
                    path = Path(parent) / name
                    relative = path.relative_to(source).as_posix()
                    if any(character in relative for character in ('\\', ':')):
                        raise ValueError(f'Invalid portable archive path: {relative}')
                    info = path.lstat()
                    if not (stat.S_ISDIR(info.st_mode) or stat.S_ISREG(info.st_mode)):
                        raise ValueError(f'Expected a regular file or directory: {relative}')
                    if os.path.samestat(info, key_identity):
                        raise ValueError(f'Signing key is inside the plugin payload: {relative}')

                    entry = archive.gettarinfo(str(path), relative)
                    entry.uid = entry.gid = entry.mtime = 0
                    entry.uname = entry.gname = ''
                    entry.mode &= 0o755
                    if stat.S_ISDIR(info.st_mode):
                        if not entry.isdir():
                            raise ValueError(f'Directory changed during packaging: {relative}')
                        archive.addfile(entry)
                    else:
                        with path.open('rb') as content:
                            before = os.fstat(content.fileno())
                            if (
                                not entry.isfile() or entry.size != info.st_size
                                or file_identity(info) != file_identity(before)
                            ):
                                raise ValueError(f'File changed during packaging: {relative}')
                            archive.addfile(entry, content)
                            if file_identity(before) != file_identity(os.fstat(content.fileno())):
                                raise ValueError(f'File changed during packaging: {relative}')
                        file_bytes += entry.size
                    count += 1
    if not count:
        raise ValueError('The plugin payload directory is empty')
    return count, file_bytes


def sign_manifest(args, archive, entries, unpacked_size, key):
    manifest = {
        'plugin_id': args.plugin_id, 'version': args.version, 'os': args.target_os,
        'arch': args.target_arch, 'archive_bytes': archive.stat().st_size,
        'archive_sha256': file_hash(archive), 'unpacked_file_bytes': unpacked_size,
        'entries': entries,
    }
    canonical = json.dumps(['plugin-archive-v1', *manifest.values()], ensure_ascii=False, separators=(',', ':'))
    signature = subprocess.check_output(
        ['node', '-e', SIGN_MANIFEST, '--', str(key)], input=canonical.encode('utf-8'),
    ).decode('ascii')
    if not re.fullmatch(r'[0-9a-f]{128}', signature):
        raise ValueError('Unexpected Ed25519 signature output')
    manifest['ed25519_signature_hex'] = signature
    return manifest


def publish(package, destination, manifest):
    if not os.path.lexists(destination):
        package.rename(destination)
        return
    if not destination.is_symlink() and destination.is_dir():
        existing = list(destination.iterdir())
        if not existing:
            destination.rmdir()
            package.rename(destination)
            return
        if {item.name for item in existing} == PACKAGE_FILES and all(
            not item.is_symlink() and item.is_file() for item in existing
        ):
            same_manifest = (destination / 'manifest.json').read_bytes() == (package / 'manifest.json').read_bytes()
            if same_manifest and file_hash(destination / 'archive.tar.gz') == manifest['archive_sha256']:
                return
    raise ValueError(f'Output already contains files that differ from this signed plugin: {destination}')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('source', type=Path, help='prepared plugin directory')
    parser.add_argument('output', type=Path, help='output directory for archive.tar.gz and manifest.json')
    parser.add_argument('--plugin-id', required=True)
    parser.add_argument('--version', required=True)
    parser.add_argument('--target-os', required=True, choices=('macos', 'linux', 'windows'))
    parser.add_argument('--target-arch', required=True, choices=('aarch64', 'x86_64', 'arm', 'x86'))
    parser.add_argument('--signing-key', type=Path, required=True, help='Ed25519 PEM private key')
    args = parser.parse_args()
    for name in (args.plugin_id, args.version):
        if not re.fullmatch(r'[A-Za-z0-9_.@/+-]+', name) or name in (
            '.', '.install.lock', '.installed-v1.json',
        ) or '..' in name:
            parser.error('invalid plugin identifier or version')

    cwd = Path(os.environ.get('CORDIS_PLUGIN_PACK_CWD', '.'))
    source = (cwd / args.source).resolve(strict=True)
    output = cwd / args.output
    output = output.parent.resolve() / output.name
    key = (cwd / args.signing_key).resolve(strict=True)
    if not source.is_dir() or not key.is_file():
        parser.error('provide a plugin directory and a regular Ed25519 key file')
    if output.is_relative_to(source) or key.is_relative_to(source):
        parser.error('keep the output directory and signing key outside the plugin payload')
    output.parent.mkdir(parents=True, exist_ok=True)
    with TemporaryDirectory(prefix='.cordis-plugin-', dir=output.parent) as temporary:
        package = Path(temporary) / 'package'
        package.mkdir()
        archive = package / 'archive.tar.gz'
        entries, unpacked_size = pack(source, archive, key)
        manifest = sign_manifest(args, archive, entries, unpacked_size, key)
        (package / 'manifest.json').write_text(json.dumps(manifest, indent=2) + '\n', encoding='utf-8')
        publish(package, output, manifest)
    print(output)


if __name__ == '__main__':
    try:
        main()
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        sys.exit(f'Could not package Cordis plugin: {error}')
