"""Stamp Desktop only in a CI checkout; retain Gateway's independent version."""
import os
from pathlib import Path
import re

manifest = Path('desktop/Cargo.toml')
source = manifest.read_text()
match = re.search(r'^version = "(\d+\.\d+\.\d+)"$', source, re.M)
if not match:
    raise SystemExit('Desktop preview requires a stable source version')
major, minor, patch = map(int, match[1].split('.'))
version = os.environ.get('DESKTOP_PREVIEW_VERSION')
if not version:
    run = os.environ['GITHUB_RUN_NUMBER']
    if not re.fullmatch(r'[1-9][0-9]*', run):
        raise SystemExit('Invalid run number')
    version = f'{major}.{minor}.{patch + 1}-preview.{run}'
if not re.fullmatch(r'\d+\.\d+\.\d+-preview\.[1-9][0-9]*', version):
    raise SystemExit('Invalid Desktop preview version')
lock = Path('Cargo.lock')
old = f'name = "muqun-desktop"\nversion = "{match[1]}"'
contents = lock.read_text()
if contents.count(old) != 1:
    raise SystemExit('Expected exactly one Desktop lock entry')
if not Path('release-notes/desktop-unreleased.md').read_text().strip():
    raise SystemExit('Desktop release notes required')
manifest.write_text(source[:match.start(1)] + version + source[match.end(1):])
lock.write_text(contents.replace(old, f'name = "muqun-desktop"\nversion = "{version}"'))
if output := os.environ.get('GITHUB_OUTPUT'):
    with open(output, 'a') as stream:
        stream.write(f'version={version}\n')
print(version)
