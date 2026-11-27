#!/usr/bin/env python3
"""Complete a GN mksnapshot.zip with the files used by @electron/mksnapshot."""

import argparse
from copy import copy
from pathlib import Path
import shutil
import subprocess
from tempfile import TemporaryDirectory
from zipfile import ZIP_DEFLATED, ZipFile, ZipInfo


ARGS = 'mksnapshot_args'
EMBEDDED = 'gen/v8/embedded.S'


def snapshot_metadata(gn, out_dir, members):
  try:
    output = subprocess.check_output(
        [gn, 'desc', out_dir, 'v8:run_mksnapshot_default', 'args'], text=True
    )
  except subprocess.CalledProcessError as error:
    raise RuntimeError(f'GN could not describe the snapshot target:\n{error.output}') from error
  lines = iter(output.splitlines())
  command = next(
      (line for line in lines if line.startswith('./')
       and Path(line).name in ('mksnapshot', 'mksnapshot.exe')),
      None,
  )
  if command is None:
    raise ValueError('GN did not describe a mksnapshot command.')
  binary = Path(command).name
  if binary in members:
    command = './' + binary
  elif command.removeprefix('./') not in members:
    raise ValueError(f'Snapshot executable is missing from the archive: {command}')

  args = [command]
  embedded = None
  for line in lines:
    if line in ('--turbo-profiling-input', '--builtins-effects-src'):
      # The build-only flags each take a separate path.
      next(lines)
    elif line == '--embedded_src':
      embedded = (out_dir / next(lines)).read_bytes()
      args.extend((line, EMBEDDED))
    elif line not in (
        '--reorder-builtins',
        '--warn-about-builtin-profile-data',
        '--abort-on-bad-builtin-profile-data',
    ):
      args.append(line)
  if not embedded:
    raise ValueError('GN did not produce an embedded V8 assembly file.')
  return {ARGS: ('\n'.join(args) + '\n').encode(), EMBEDDED: embedded}


def update_archive(archive_path, metadata):
  # Build the entire update beside the archive so an interrupted build can retry.
  with TemporaryDirectory(prefix='.mksnapshot-', dir=archive_path.parent) as folder:
    updated = Path(folder) / archive_path.name
    with ZipFile(archive_path) as existing:
      names = existing.namelist()
      if all(names.count(name) == 1 and existing.read(name) == data
             for name, data in metadata.items()):
        return
      if metadata.keys().isdisjoint(names):
        shutil.copyfile(archive_path, updated)
      else:
        with ZipFile(updated, 'w') as replacement:
          replacement.comment = existing.comment
          for entry in existing.infolist():
            if entry.filename not in metadata:
              with existing.open(entry) as source, replacement.open(copy(entry), 'w') as target:
                shutil.copyfileobj(source, target)
    with ZipFile(updated, 'a') as archive:
      for name, data in metadata.items():
        archive.writestr(ZipInfo(name), data, compress_type=ZIP_DEFLATED)
    shutil.copymode(archive_path, updated)
    updated.replace(archive_path)


def main():
  parser = argparse.ArgumentParser(description=__doc__)
  parser.add_argument('gn', help='GN executable')
  parser.add_argument('out_dir', type=Path, help='GN build directory')
  args = parser.parse_args()
  archive_path = args.out_dir / 'mksnapshot.zip'
  with ZipFile(archive_path) as archive:
    members = set(archive.namelist())
  update_archive(archive_path, snapshot_metadata(args.gn, args.out_dir, members))


if __name__ == '__main__':
  main()
