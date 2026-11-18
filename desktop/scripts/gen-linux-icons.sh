#!/usr/bin/env bash
# Regenerate the Linux launcher icons from the app icon (assets/icons/app-icon.png).
# Preserve the transparent canvas at each launcher size. Requires ImageMagick 7.
set -euo pipefail
DESKTOP="$(cd "$(dirname "$0")/.." && pwd)"
SRC="$DESKTOP/assets/icons/app-icon.png"
OUT="$DESKTOP/packaging/linux/icons/hicolor"
command -v magick >/dev/null || { echo "needs ImageMagick 7 (magick)" >&2; exit 1; }

for size in 16 24 32 48 64 128 256 512; do
  mkdir -p "$OUT/${size}x${size}/apps"
  magick "$SRC" -filter Lanczos -resize "${size}x${size}" -strip \
    "PNG32:$OUT/${size}x${size}/apps/muqun-desktop.png"
done
echo "icons written under $OUT"
