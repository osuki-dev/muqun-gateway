#!/usr/bin/env bash
# Assemble a local Linux candidate. This does not publish or tag a release.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
[[ "$(uname -s)" == Linux ]] || { echo 'Linux packaging requires Linux' >&2; exit 1; }
if [[ "${1:-}" != --no-build ]]; then
  [[ $# == 0 ]] || { echo 'usage: package-linux.sh [--no-build]' >&2; exit 1; }
  cargo build --manifest-path "$ROOT/Cargo.toml" -p muqun-desktop --release --locked
fi
[[ $# -le 1 ]] || exit 1
BIN="${CARGO_TARGET_DIR:-$ROOT/target}/release/muqun-desktop"
OUT="${CARGO_TARGET_DIR:-$ROOT/target}/desktop-package"
mkdir -p "$OUT"
STAGE="$(mktemp -d "$OUT/stage.XXXXXX")"
trap 'rm -rf -- "$STAGE"' EXIT
NAME="muqun-desktop-linux-$(uname -m)"
PKG="$STAGE/$NAME"
mkdir -p "$PKG/bin" "$PKG/desktop/scripts" "$PKG/desktop/packaging" "$PKG/desktop/assets/plugins/review-center"
cp "$BIN" "$PKG/bin/muqun-desktop"
cp "$ROOT/desktop/scripts/install-local.sh" "$ROOT/desktop/scripts/uninstall-local.sh" "$PKG/desktop/scripts/"
cp -R "$ROOT/desktop/packaging/linux" "$PKG/desktop/packaging/"
cp "$ROOT/desktop/assets/plugins/review-center/plugin.json" "$ROOT/desktop/assets/plugins/review-center/review.ts" "$ROOT/desktop/assets/plugins/review-center/README.md" "$PKG/desktop/assets/plugins/review-center/"
cp "$ROOT/desktop/README.md" "$PKG/README.md"
git -C "$ROOT" rev-parse HEAD > "$PKG/SOURCE"
if [[ -n "$(git -C "$ROOT" status --porcelain)" ]]; then
  printf 'dirty working tree; local candidate only\n' >> "$PKG/SOURCE"
fi
tar -czf "$OUT/$NAME.tar.gz" -C "$STAGE" "$NAME"
(cd "$OUT" && sha256sum "$NAME.tar.gz" > "$NAME.tar.gz.sha256")
printf 'Created %s\nInstall after extraction: bash %s/desktop/scripts/install-local.sh --no-build\n' "$OUT/$NAME.tar.gz" "$NAME"
