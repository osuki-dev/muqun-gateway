#!/usr/bin/env bash
# Exercise the actual signed ZIP and its installer before publishing. Only
# HTTPS transport is replaced with the not-yet-published files from this run.
set -euo pipefail
zip="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
archive=$(basename "$zip")
version=${archive#muqun-desktop-}
version=${version%-macos-arm64.zip}
directory=$(mktemp -d)
trap 'rm -rf "$directory"' EXIT
bin="$directory/path"
export HOME="$directory/home" MUQUN_DESKTOP_APPLICATIONS_DIR="$directory/apps with spaces"
export TMPDIR="$directory/tmp" RELEASE_FIXTURE_DIR="$directory/http"
mkdir -p "$bin" "$HOME" "$TMPDIR" "$RELEASE_FIXTURE_DIR"
for tool in awk cat chmod codesign cp dirname ditto mkdir mktemp plutil rm sh shasum spctl touch uname unzip wc; do
  ln -s "$(command -v "$tool")" "$bin/$tool"
done
ln -s "$zip" "$RELEASE_FIXTURE_DIR/$archive"
ln -s "$zip.sha256" "$RELEASE_FIXTURE_DIR/$archive.sha256"
size() { wc -c < "$1" | awk '{print $1}'; }
digest() { shasum -a 256 < "$1" | awk '{print $1}'; }
printf '%s %s %s %s\n' "$(size "$zip")" "$(digest "$zip")" "$(size "$zip.sha256")" "$(digest "$zip.sha256")" > "$RELEASE_FIXTURE_DIR/$archive.metadata"
cat > "$bin/curl" <<'CURL'
#!/bin/sh
set -eu
while [ "$1" != -o ]; do shift; done
cp "$RELEASE_FIXTURE_DIR/${3##*/}" "$2"
CURL
chmod +x "$bin/curl"
unzip -p "$zip" 'Muqun Desktop.app/Contents/Resources/installers/install-desktop.sh' |
  PATH="$bin" sh -s -- --tag "desktop-v$version"
app="$MUQUN_DESKTOP_APPLICATIONS_DIR/Muqun Desktop.app"
test ! -e "$HOME/Library/Application Support/muqun-desktop/gateway-auto-install.disabled"
codesign --verify --strict "$app/Contents/Helpers/muqun-desktop-install"
codesign --verify --strict "$app/Contents/MacOS/muqun-wry-embedded"
spctl --assess --type execute "$app"
