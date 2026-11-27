#!/usr/bin/env python3

import subprocess
import re
from pathlib import Path

# Find the nearest tag to the current HEAD.
# This is equivalent to our old logic of "use a value in package.json" for the
# following reasons:
#
# 1. Whenever we updated the package.json we ALSO pushed a tag with the same
#    version.
# 2. Whenever we _reverted_ a bump all we actually did was push a commit that
#    deleted the tag and changed the version number back.
#
# The only difference in the "git describe" technique is that technically a
# commit can "change" its version number if a tag is created / removed
# retroactively.  i.e. the first time a commit is pushed it will be 1.2.3
# and after the tag is made rebuilding the same commit will result in it being
# 1.2.4.

electron_dir = Path(__file__).resolve().parent.parent

try:
  output = subprocess.check_output(
      ['git', 'describe', '--tags', '--abbrev=0'],
      cwd=electron_dir,
      stderr=subprocess.PIPE,
      universal_newlines=True)
  # only remove the 'v' prefix from the tag name.
  version = re.sub('^v', '', output.strip())
  print(version)
except Exception:
  # Cordis clones may omit the upstream release tags. Keep the version from
  # the pinned Electron base so packagers resolve the matching native headers.
  upstream_version = electron_dir / 'build' / 'electron-cordis-upstream-version'
  if upstream_version.is_file():
    print(upstream_version.read_text(encoding='utf-8').strip())
  else:
    print('0.0.0-no-git-tag-found')
