#!/usr/bin/env bash
# Regenerate the Linux launcher icons from the app icon (assets/icons/app-icon.png).
# The full-bleed square becomes a rounded tile with a small margin, as desktop
# launchers draw app icons, then one PNG per hicolor size. Needs ImageMagick 7
# (`magick`); the output is committed, so installing never needs it.
set -euo pipefail
DESKTOP="$(cd "$(dirname "$0")/.." && pwd)"
SRC="$DESKTOP/assets/icons/app-icon.png"
OUT="$DESKTOP/packaging/linux/icons/hicolor"
command -v magick >/dev/null || { echo "needs ImageMagick 7 (magick)" >&2; exit 1; }

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
# 1024 canvas, 896 tile (6.25% margin), corner radius 22% of the tile.
magick -size 896x896 xc:none -fill white \
  -draw "roundrectangle 0,0,895,895,197,197" "$work/mask.png"
magick "$SRC" -resize 896x896 -alpha set "$work/mask.png" \
  -compose DstIn -composite \
  -compose Over -background none -gravity center -extent 1024x1024 \
  "$work/master.png"

for size in 16 24 32 48 64 128 256 512; do
  mkdir -p "$OUT/${size}x${size}/apps"
  magick "$work/master.png" -filter Lanczos -resize "${size}x${size}" -strip \
    "PNG32:$OUT/${size}x${size}/apps/muqun-desktop.png"
done
echo "icons written under $OUT"
