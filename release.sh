#!/usr/bin/env sh
# Cut a release: bump the version, tag, and push. The tag push triggers the CI
# that builds the binaries and attaches them to the GitHub release.
#
#   ./release.sh patch          0.3.0 -> 0.3.1
#   ./release.sh minor          0.3.0 -> 0.4.0
#   ./release.sh major          0.3.0 -> 1.0.0
#   ./release.sh 0.5.2          set an explicit version
set -eu

die() { printf '\033[1;31mError:\033[0m %s\n' "$1" >&2; exit 1; }
info() { printf '\033[1;36m==>\033[0m %s\n' "$1"; }

[ "$(git rev-parse --abbrev-ref HEAD)" = "main" ] || die "release from main only."
[ -z "$(git status --porcelain)" ] || die "working tree is not clean; commit or stash first."

current="$(grep -m1 '^version' Cargo.toml | sed 's/[^"]*"\([^"]*\)".*/\1/')"
maj="${current%%.*}"
rest="${current#*.}"
min="${rest%%.*}"
pat="${rest#*.}"

case "${1:-}" in
  major) next="$((maj + 1)).0.0" ;;
  minor) next="$maj.$((min + 1)).0" ;;
  patch) next="$maj.$min.$((pat + 1))" ;;
  [0-9]*.[0-9]*.[0-9]*) next="$1" ;;
  *) die "usage: release.sh patch|minor|major|X.Y.Z (current: $current)" ;;
esac

tag="v$next"
git rev-parse "$tag" >/dev/null 2>&1 && die "tag $tag already exists."

info "Bumping $current -> $next"
sed -i.bak "s/^version = \"$current\"/version = \"$next\"/" Cargo.toml herdr-plugin.toml
rm -f Cargo.toml.bak herdr-plugin.toml.bak

info "Rebuilding so Cargo.lock and tests reflect the new version"
cargo build --release --locked >/dev/null
cargo test >/dev/null

git add Cargo.toml herdr-plugin.toml Cargo.lock
git commit -m "release: $tag"
git tag -a "$tag" -m "$tag"

info "Pushing main and $tag (this kicks off the release build)"
git push origin main "$tag"

printf '\033[1;32mReleased %s.\033[0m CI is now building the binaries: https://github.com/osuki-dev/muqun-gateway/actions\n' "$tag"
