#!/usr/bin/env bash
# Install a release muqun-desktop on this machine: binary, icon, launcher entry.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
BIN_SRC="${1:-$ROOT/target/release/muqun-desktop}"
BIN_DST="${HOME}/.local/bin/muqun-desktop"
ICON_SRC="$ROOT/desktop/assets/icons/app-icon.png"
APP_ID="muqun"
DESKTOP="${HOME}/.local/share/applications/${APP_ID}.desktop"

if [[ ! -x "$BIN_SRC" ]]; then
  echo "missing release binary: $BIN_SRC" >&2
  echo "build first: cargo build -p muqun-desktop --release --locked" >&2
  exit 1
fi

mkdir -p "$(dirname "$BIN_DST")" \
  "${HOME}/.local/share/applications" \
  "${HOME}/.local/share/icons/hicolor/256x256/apps" \
  "${HOME}/.local/share/icons/hicolor/128x128/apps" \
  "${HOME}/.local/share/icons/hicolor/512x512/apps" \
  "${HOME}/.local/share/icons/hicolor/scalable/apps"

install -m 755 "$BIN_SRC" "$BIN_DST"

if command -v magick >/dev/null 2>&1; then
  magick "$ICON_SRC" -resize 128x128 -strip \
    "${HOME}/.local/share/icons/hicolor/128x128/apps/${APP_ID}.png"
  magick "$ICON_SRC" -resize 256x256 -strip \
    "${HOME}/.local/share/icons/hicolor/256x256/apps/${APP_ID}.png"
  magick "$ICON_SRC" -resize 512x512 -strip \
    "${HOME}/.local/share/icons/hicolor/512x512/apps/${APP_ID}.png"
else
  cp -f "$ICON_SRC" "${HOME}/.local/share/icons/hicolor/512x512/apps/${APP_ID}.png"
fi
# Full mark for any size lookup that falls back to "scalable".
cp -f "$ICON_SRC" "${HOME}/.local/share/icons/hicolor/scalable/apps/${APP_ID}.png"

cat >"$DESKTOP" <<EOF
[Desktop Entry]
Type=Application
Name=Muqun
GenericName=Muqun
Comment=Muqun — coding agents on this computer
Exec=${BIN_DST}
TryExec=${BIN_DST}
Icon=${APP_ID}
Terminal=false
Categories=Development;IDE;
StartupNotify=true
StartupWMClass=muqun-desktop
Keywords=muqun;agent;coding;
EOF
chmod 644 "$DESKTOP"

if command -v update-desktop-database >/dev/null 2>&1; then
  update-desktop-database "${HOME}/.local/share/applications" >/dev/null 2>&1 || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
  gtk-update-icon-cache -f -t "${HOME}/.local/share/icons/hicolor" >/dev/null 2>&1 || true
fi

echo "installed: $BIN_DST"
echo "launcher:  $DESKTOP"
echo "icon:      ${APP_ID} (hicolor)"
echo "run:       muqun-desktop   # or open Muqun from the app launcher"
