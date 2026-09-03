#!/usr/bin/env sh
# Cut a release: bump the version, open the pull request, merge it, tag the
# merged commit, and push the tag. The tag push triggers the CI that builds the
# binaries and attaches them to the GitHub release.
#
#   ./release.sh patch          0.3.0 -> 0.3.1
#   ./release.sh minor          0.3.0 -> 0.4.0
#   ./release.sh major          0.3.0 -> 1.0.0
#   ./release.sh 0.5.2          set an explicit version
#   ./release.sh --tag          finish a release whose pull request is already merged
#
# ## Why this is not four git commands any more
#
# It used to bump, commit, tag, and `git push origin main "$tag"`. `main` is a
# protected branch and rejects a direct push, so on v0.8.0 that last command
# half-succeeded: git pushes refs one at a time and the tag went up while `main`
# was declined. CI built and published the release from a commit that was not on
# `main` and never would be, and the version bump survived only in the local
# working copy. A release that exists on the tag and nowhere else is not a state
# anyone should have to recognise and repair by hand.
#
# So the bump goes through the same review path as every other change, and the
# tag is cut from the commit the merge actually produced. The order is the whole
# point: `main` first, tag second. A tag that is an ancestor of `main` can be
# reasoned about; one that dangles beside it cannot.
#
# The merge is squashed, so the commit this script pushes a branch for is not
# the commit that ends up on `main`. That is why the tag is taken from
# `origin/main` after the merge rather than from the local commit -- tagging
# what we pushed would recreate the v0.8.0 split with extra steps.
set -eu

die() { printf '\033[1;31mError:\033[0m %s\n' "$1" >&2; exit 1; }
info() { printf '\033[1;36m==>\033[0m %s\n' "$1"; }

command -v gh >/dev/null 2>&1 || die "the GitHub CLI (gh) is required; see https://cli.github.com"
[ "$(git rev-parse --abbrev-ref HEAD)" = "main" ] || die "release from main only."
[ -z "$(git status --porcelain)" ] || die "working tree is not clean; commit or stash first."

# Tag whatever `main` is now and stop. The resume path for a pull request that
# needed a human -- a required review, a check this script cannot satisfy --
# where the merge happened after the script gave up.
if [ "${1:-}" = "--tag" ]; then
  git fetch origin main --quiet
  git merge --ff-only origin/main >/dev/null 2>&1 || die "local main is not origin/main; pull first."
  version="$(grep -m1 '^version' Cargo.toml | sed 's/[^"]*"\([^"]*\)".*/\1/')"
  tag="v$version"
  git rev-parse "$tag" >/dev/null 2>&1 && die "tag $tag already exists."
  info "Tagging $(git rev-parse --short HEAD) as $tag"
  git tag -a "$tag" -m "$tag"
  git push origin "$tag"
  printf '\033[1;32mReleased %s.\033[0m CI is now building the binaries: https://github.com/osuki-dev/muqun-gateway/actions\n' "$tag"
  exit 0
fi

# The pull request is opened against whatever `origin/main` is, so a stale local
# main would put a bump on top of commits this checkout has never seen and the
# tag would be cut from a merge nobody here verified.
git fetch origin main --quiet
[ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ] || die "main is not in sync with origin/main; pull first."

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
  *) die "usage: release.sh patch|minor|major|X.Y.Z|--tag (current: $current)" ;;
esac

tag="v$next"
branch="release/$tag"
git rev-parse "$tag" >/dev/null 2>&1 && die "tag $tag already exists."
git ls-remote --exit-code --tags origin "$tag" >/dev/null 2>&1 && die "tag $tag already exists on the remote."

# Before the bump, while Cargo.lock still describes this tree exactly.
#
# The build below is `--offline` so that no dependency can quietly move during a
# release, and that guarantee is worth keeping -- but it also fails outright
# when a crate added since the last local build is not in the registry cache,
# which is how v0.8.0 first died on `aead v0.5.2`. Fetching the locked set first
# is not a hole in the guarantee: `--locked` refuses to resolve anything the
# lockfile does not already name, so this downloads exactly what is pinned and
# nothing else.
info "Priming the registry cache from Cargo.lock"
cargo fetch --locked >/dev/null

info "Bumping $current -> $next on $branch"
git checkout -q -b "$branch"
sed -i.bak "s/^version = \"$current\"/version = \"$next\"/" Cargo.toml herdr-plugin.toml
rm -f Cargo.toml.bak herdr-plugin.toml.bak

info "Rebuilding so Cargo.lock and tests reflect the new version"
# Not `--locked`. Cargo.lock records this package's own version, so the bump two
# lines above always invalidates it, and `--locked` exists to refuse exactly
# that -- it made this step fail on every release where the version actually
# changed. `--offline` keeps the guarantee that was wanted here: no registry
# access, so no dependency can quietly move; only the local version entry is
# rewritten, which is the point of running this at all.
cargo build --release --offline >/dev/null
cargo test --offline >/dev/null

git add Cargo.toml herdr-plugin.toml Cargo.lock
git commit -q -m "release: $tag"
git push -q -u origin "$branch"

info "Opening the pull request"
gh pr create --base main --head "$branch" \
  --title "release: $tag" \
  --body "The version bump for \`$tag\`. \`main\` is protected, so the bump goes through review like everything else; the tag is cut from the merge commit once this lands." \
  >/dev/null

info "Merging it"
if ! gh pr merge "$branch" --squash --delete-branch >/dev/null 2>&1; then
  git checkout -q main
  die "the pull request could not be merged automatically -- a review or a check is probably waiting.
  Merge https://github.com/osuki-dev/muqun-gateway/pulls yourself, then run:  ./release.sh --tag"
fi

# The tag belongs to the squashed commit on main, not to the one pushed above.
git checkout -q main
git fetch origin main --quiet
git merge --ff-only origin/main >/dev/null 2>&1 \
  || die "main moved underneath this release; reconcile it, then run: ./release.sh --tag"

info "Tagging $(git rev-parse --short HEAD) as $tag"
git tag -a "$tag" -m "$tag"
git push origin "$tag"

printf '\033[1;32mReleased %s.\033[0m CI is now building the binaries: https://github.com/osuki-dev/muqun-gateway/actions\n' "$tag"
