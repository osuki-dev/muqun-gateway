#!/usr/bin/env bash
# Build Muqun Desktop in release mode and install it for this user: the binary,
# the launcher entry and the icons. No sudo; running it again updates in place.
#
#   desktop/scripts/install-local.sh              # build, then install under ~/.local
#   desktop/scripts/install-local.sh --no-build   # install target/release as it is
#   PREFIX=/tmp/try desktop/scripts/install-local.sh
#
# Installs $PREFIX/bin/muqun-desktop, $PREFIX/share/applications/muqun-desktop.desktop
# and $PREFIX/share/icons/hicolor/<size>/apps/muqun-desktop.png (PREFIX defaults to
# ~/.local). Remove it all with uninstall-local.sh.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
PACKAGING="$ROOT/desktop/packaging/linux"
PREFIX="${PREFIX:-$HOME/.local}"
BIN_DIR="$PREFIX/bin"
APPS_DIR="$PREFIX/share/applications"
ICONS_DIR="$PREFIX/share/icons/hicolor"
APP_ID="muqun-desktop"
BUILD=1

for arg in "$@"; do
  case "$arg" in
    --no-build) BUILD=0 ;;
    -h | --help)
      sed -n '2,11p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      echo "unknown option: $arg (see --help)" >&2
      exit 2
      ;;
  esac
done

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "install-local.sh installs the Linux launcher entry; on $(uname -s) run target/release/muqun-desktop" >&2
  exit 1
fi

if [[ "$BUILD" == 1 ]]; then
  echo "building muqun-desktop (release)…"
  cargo build --manifest-path "$ROOT/Cargo.toml" -p muqun-desktop --release --locked
fi
# cargo puts the build where CARGO_TARGET_DIR says, when it is set.
BIN_SRC="${CARGO_TARGET_DIR:-$ROOT/target}/release/muqun-desktop"
if [[ "$BUILD" == 0 && -x "$ROOT/bin/muqun-desktop" ]]; then
  BIN_SRC="$ROOT/bin/muqun-desktop"
fi
if [[ ! -x "$BIN_SRC" ]]; then
  echo "missing release binary: $BIN_SRC (run without --no-build)" >&2
  exit 1
fi

mkdir -p "$BIN_DIR" "$APPS_DIR"

# Copy next to the target, then rename over it: a running Muqun keeps its old
# binary until it restarts, and a half-written file is never what launches.
tmp="$BIN_DIR/.$APP_ID.new.$$"
cp "$BIN_SRC" "$tmp"
chmod 755 "$tmp"
mv -f "$tmp" "$BIN_DIR/$APP_ID"

for dir in "$PACKAGING"/icons/hicolor/*/apps; do
  size="$(basename "$(dirname "$dir")")"
  mkdir -p "$ICONS_DIR/$size/apps"
  install -m 644 "$dir/$APP_ID.png" "$ICONS_DIR/$size/apps/$APP_ID.png"
done

# Package files never overwrite the separate user plugins/ directory.
PLUGIN_DIR="$PREFIX/share/$APP_ID/bundled-plugins/review-center"
mkdir -p "$PLUGIN_DIR"
install -m 644 "$ROOT/desktop/assets/plugins/review-center/plugin.json" "$PLUGIN_DIR/plugin.json"
install -m 644 "$ROOT/desktop/assets/plugins/review-center/review.ts" "$PLUGIN_DIR/review.ts"
install -m 644 "$ROOT/desktop/assets/plugins/review-center/README.md" "$PLUGIN_DIR/README.md"

# The launcher runs the installed binary by its full path, so it works even
# when $PREFIX/bin is not on the session's PATH.
exec_path="$(printf '%s' "$BIN_DIR/$APP_ID" | sed 's/[\\"`$]/\\\\&/g')"
exec_replacement="$(printf '"%s"' "$exec_path" | sed 's/[\\&|]/\\&/g')"
try_replacement="$(printf '%s' "$BIN_DIR/$APP_ID" | sed 's/[\\&|]/\\&/g')"
sed -e "s|^Exec=muqun-desktop|Exec=$exec_replacement|" \
  -e "s|^TryExec=muqun-desktop|TryExec=$try_replacement|" \
  "$PACKAGING/$APP_ID.desktop" >"$APPS_DIR/$APP_ID.desktop"
chmod 644 "$APPS_DIR/$APP_ID.desktop"

# An earlier script installed the same app as `muqun.desktop` / `muqun.png`;
# two launcher entries for one app is one too many.
legacy="$APPS_DIR/muqun.desktop"
if [[ -f "$legacy" ]] && grep -q "muqun-desktop" "$legacy"; then
  rm -f "$legacy"
  for icon in "$ICONS_DIR"/*/apps/muqun.png; do
    [[ -e "$icon" ]] && rm -f "$icon"
  done
fi

if command -v desktop-file-validate >/dev/null 2>&1; then
  desktop-file-validate "$APPS_DIR/$APP_ID.desktop" || true
fi
if command -v update-desktop-database >/dev/null 2>&1; then
  update-desktop-database "$APPS_DIR" >/dev/null 2>&1 || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1 && [[ -f "$ICONS_DIR/index.theme" ]]; then
  gtk-update-icon-cache -f -t "$ICONS_DIR" >/dev/null 2>&1 || true
fi

echo "installed: $BIN_DIR/$APP_ID"
echo "launcher:  $APPS_DIR/$APP_ID.desktop"
echo "icons:     $ICONS_DIR/<size>/apps/$APP_ID.png"
case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) echo "note: $BIN_DIR is not on PATH; the launcher entry works regardless" ;;
esac
if pgrep -x "$APP_ID" >/dev/null 2>&1; then
  echo "note: Muqun is running; quit and reopen it to use this build"
fi
