#!/usr/bin/env bash
# Remove what install-local.sh installed: the binary, the launcher entry and the
# icons. Settings and session state stay (~/.config/muqun-desktop,
# ~/.local/share/muqun-desktop); delete those by hand to start over.
#
#   desktop/scripts/uninstall-local.sh
#   PREFIX=/tmp/try desktop/scripts/uninstall-local.sh
set -euo pipefail

PREFIX="${PREFIX:-$HOME/.local}"
BIN_DIR="$PREFIX/bin"
APPS_DIR="$PREFIX/share/applications"
ICONS_DIR="$PREFIX/share/icons/hicolor"
APP_ID="muqun-desktop"

removed=0
remove() {
  if [[ -e "$1" || -L "$1" ]]; then
    rm -f "$1"
    echo "removed: $1"
    removed=$((removed + 1))
  fi
}

PLUGIN_DIR="$PREFIX/share/$APP_ID/bundled-plugins/review-center"
remove "$PLUGIN_DIR/plugin.json"
remove "$PLUGIN_DIR/review.ts"
remove "$PLUGIN_DIR/README.md"
if [[ -d "$PLUGIN_DIR" ]]; then
  rmdir "$PLUGIN_DIR" 2>/dev/null || true
fi

remove "$BIN_DIR/$APP_ID"
remove "$APPS_DIR/$APP_ID.desktop"
for icon in "$ICONS_DIR"/*/apps/"$APP_ID".png; do
  remove "$icon"
done
# The entry an earlier install script wrote, when it is ours.
legacy="$APPS_DIR/muqun.desktop"
if [[ -f "$legacy" ]] && grep -q "muqun-desktop" "$legacy"; then
  remove "$legacy"
  for icon in "$ICONS_DIR"/*/apps/muqun.png; do
    remove "$icon"
  done
fi

if command -v update-desktop-database >/dev/null 2>&1 && [[ -d "$APPS_DIR" ]]; then
  update-desktop-database "$APPS_DIR" >/dev/null 2>&1 || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1 && [[ -f "$ICONS_DIR/index.theme" ]]; then
  gtk-update-icon-cache -f -t "$ICONS_DIR" >/dev/null 2>&1 || true
fi

if [[ "$removed" == 0 ]]; then
  echo "nothing to remove under $PREFIX"
fi
echo "kept: settings and state (~/.config/$APP_ID, ~/.local/share/$APP_ID)"
