#!/usr/bin/env bash
# Shared, portable release build for local installation and CI.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
[[ $# == 0 ]] || { echo 'usage: build-release.sh' >&2; exit 1; }
cd "$ROOT"
# Ghostty pins Zig 0.15.2. Prefer an explicitly selected compatible binary,
# then the existing mise installation; never download or change global tools.
ZIG_VERSION="$(zig version 2>/dev/null || true)"
if [[ "$ZIG_VERSION" != 0.15.2 ]]; then
  MISE_ZIG="${MISE_DATA_DIR:-$HOME/.local/share/mise}/installs/zig/0.15.2"
  if [[ -x "$MISE_ZIG/zig" ]]; then
    export PATH="$MISE_ZIG:$PATH"
  elif [[ -x "$MISE_ZIG/bin/zig" ]]; then
    export PATH="$MISE_ZIG/bin:$PATH"
  fi
fi
[[ "$(zig version 2>/dev/null || true)" == 0.15.2 ]] || {
  echo 'Desktop requires Zig 0.15.2; install/select it before building.' >&2
  exit 1
}
if [[ "$(uname -s)" == Darwin ]]; then
  # Zig's build runner also links macOS system libraries. An explicit SDK
  # makes the selected SDK deterministic on machines with several Xcodes.
  export SDKROOT="${SDKROOT:-$(xcrun --sdk macosx --show-sdk-path)}"
  [[ -d "$SDKROOT" ]] || { echo 'macOS SDK is missing' >&2; exit 1; }
fi
# Cargo.toml owns optimization settings. Keep portable CPU targets and unwind
# recovery; do not inject target-cpu=native, panic=abort or UPX into releases.
export CARGO_INCREMENTAL=0
cargo build --manifest-path "$ROOT/Cargo.toml" -p muqun-desktop --release --locked
