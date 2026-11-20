#!/usr/bin/env bash
# Run in the artifact directory before publishing a stable Desktop release.
set -euo pipefail
version=${1:?Expected Desktop version}
archive=muqun-desktop-linux-x86_64.tar.gz
zip="muqun-desktop-$version-macos-arm64.zip"
sha256sum -c "$archive.sha256" "$zip.sha256"
if [[ "$(tar -xzOf "$archive" muqun-desktop-linux-x86_64/VERSION)" != "$version" ]]; then
  echo "Linux archive does not contain Desktop $version" >&2
  exit 1
fi
unzip -p "$zip" 'Muqun Desktop.app/Contents/Info.plist' |
  python3 -c 'import plistlib, sys
if plistlib.loads(sys.stdin.buffer.read())["CFBundleShortVersionString"] != sys.argv[1]:
    sys.exit("macOS bundle does not contain Desktop " + sys.argv[1])' "$version"
