#!/usr/bin/env sh
# Herdr runs this as the plugin's build step (see herdr-plugin.toml). It puts a
# herdr-gateway binary at target/release/, preferring a prebuilt release asset
# so the install needs no Rust toolchain, and falling back to `cargo build` when
# no matching binary is published (e.g. an untagged main checkout).
set -eu

REPO="osuki-dev/herdr-gateway"
VERSION="$(grep -m1 '^version' Cargo.toml | sed 's/[^"]*"\([^"]*\)".*/\1/')"

os="$(uname -s)"
arch="$(uname -m)"
case "$os/$arch" in
  Darwin/arm64)         target="aarch64-apple-darwin" ;;
  Darwin/x86_64)        target="x86_64-apple-darwin" ;;
  Linux/x86_64)         target="x86_64-unknown-linux-musl" ;;
  Linux/aarch64|Linux/arm64) target="aarch64-unknown-linux-musl" ;;
  *)                    target="" ;;
esac

download() {
  [ -n "$target" ] || return 1
  command -v curl >/dev/null 2>&1 || return 1
  url="https://github.com/$REPO/releases/download/v$VERSION/herdr-gateway-$target"
  echo "Fetching prebuilt binary: $url"
  mkdir -p target/release
  curl -fSL "$url" -o target/release/herdr-gateway || return 1
  chmod +x target/release/herdr-gateway
}

if download; then
  echo "Installed the prebuilt v$VERSION binary for $target -- no build needed."
  exit 0
fi

echo "No prebuilt binary for v$VERSION ($target); building from source instead."
if ! command -v cargo >/dev/null 2>&1; then
  echo "Rust (cargo) is required to build from source. Install it from https://rustup.rs" >&2
  exit 1
fi
cargo build --release --locked
