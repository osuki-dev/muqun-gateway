#!/usr/bin/env bash
# Build a reproducible app bundle; publication is a separate step.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
[[ "$(uname -s)" == Darwin ]] || { echo 'macOS packaging requires macOS' >&2; exit 1; }
[[ $# == 0 || ( $# == 1 && "$1" == --no-build ) ]] || { echo 'usage: package-macos.sh [--no-build]' >&2; exit 1; }
if [[ $# == 0 ]]; then
  bash "$ROOT/desktop/scripts/build-release.sh"
fi
TARGET="${CARGO_TARGET_DIR:-$ROOT/target}"
OUT="$TARGET/desktop-package"
mkdir -p "$OUT"
STAGE="$(mktemp -d "$OUT/stage.XXXXXX")"
trap 'rm -rf -- "$STAGE"' EXIT
APP="$STAGE/Muqun Desktop.app"
RES="$APP/Contents/Resources"
mkdir -p "$APP/Contents/MacOS" "$RES/bundled-plugins/review-center" "$STAGE/AppIcon.iconset"
cp "$TARGET/release/muqun-desktop" "$APP/Contents/MacOS/"
cp "$ROOT/LICENSE" "$ROOT/LICENSE-MIT" "$RES/"
for file in plugin.json review.ts README.md; do
  cp "$ROOT/desktop/assets/plugins/review-center/$file" "$RES/bundled-plugins/review-center/"
done
for size in 16 32 128 256 512; do
  sips -z "$size" "$size" "$ROOT/desktop/assets/icons/app-icon.png" --out "$STAGE/AppIcon.iconset/icon_${size}x${size}.png" >/dev/null
  double=$((size * 2))
  sips -z "$double" "$double" "$ROOT/desktop/assets/icons/app-icon.png" --out "$STAGE/AppIcon.iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$STAGE/AppIcon.iconset" -o "$RES/AppIcon.icns"
VERSION="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$ROOT/desktop/Cargo.toml" | head -1)"
BUNDLE_VERSION="${VERSION%%-*}"
BUILD_NUMBER="${VERSION##*-preview.}"
if [[ "$VERSION" != *-preview.* ]]; then BUILD_NUMBER="$BUNDLE_VERSION"; fi
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>com.muqun.desktop</string>
<key>CFBundleName</key><string>Muqun Desktop</string>
<key>CFBundleExecutable</key><string>muqun-desktop</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>$BUNDLE_VERSION</string>
<key>CFBundleVersion</key><string>$BUILD_NUMBER</string>
<key>CFBundleIconFile</key><string>AppIcon</string>
<key>NSHighResolutionCapable</key><true/>
<key>NSMicrophoneUsageDescription</key><string>Record speech and transcribe it into your message draft.</string>
</dict></plist>
PLIST
plutil -lint "$APP/Contents/Info.plist"
git -C "$ROOT" rev-parse HEAD > "$RES/SOURCE"
if [[ -n "$(git -C "$ROOT" status --porcelain)" ]]; then
  printf 'dirty working tree; local candidate only\n' >> "$RES/SOURCE"
fi
if [[ -n "${MACOS_SIGN_IDENTITY:-}" ]]; then
  codesign --force --options runtime --timestamp --sign "$MACOS_SIGN_IDENTITY" "$APP"
else
  codesign --force --sign - "$APP"
  echo 'Ad-hoc candidate: not notarized for public distribution.' >&2
fi
codesign --verify --strict "$APP"
NAME="muqun-desktop-$VERSION-macos-$(uname -m).zip"
ditto -c -k --keepParent "$APP" "$OUT/$NAME"
(cd "$OUT" && shasum -a 256 "$NAME" > "$NAME.sha256")
printf 'Created %s\n' "$OUT/$NAME"
