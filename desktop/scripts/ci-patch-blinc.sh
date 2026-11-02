#!/usr/bin/env bash
# CI-only: point the root Cargo.toml [patch.crates-io] Blinc entries at the
# checked-out fork. The committed manifest carries an absolute local path
# (a developer worktree) until the fork is published; a `--config` patch
# cannot override it because the manifest's [patch] wins over config.
#
# Usage: ci-patch-blinc.sh <blinc-checkout-dir>
set -euo pipefail

dir="${1:?usage: ci-patch-blinc.sh <blinc-checkout-dir>}"
# Windows runners need a mixed path (C:/...), which TOML accepts.
if command -v cygpath >/dev/null 2>&1; then dir="$(cygpath -m "$dir")"; else dir="$(cd "$dir" && pwd)"; fi

manifest="$(dirname "$0")/../../Cargo.toml"
local_root="/home/ryu/Work/muqun/blinc-ime"
grep -q "$local_root" "$manifest" || { echo "no local Blinc patch paths found in $manifest" >&2; exit 1; }
sed -i.bak "s#$local_root#$dir#g" "$manifest"
rm -f "$manifest.bak"
grep -n 'path = ' "$manifest"
