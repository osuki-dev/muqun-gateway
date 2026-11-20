#!/bin/sh
# A single, source-free installer for published Desktop packages.
set -eu
case "${1:-}" in
  --help|-h)
    cat <<'USAGE'
Usage: sh install-desktop.sh [--channel stable|preview] [--tag desktop-vVERSION]
       sh /path/to/packaged/install-desktop.sh --gateway-only

Install a released Linux x86_64 or macOS arm64 Desktop package. The default
channel is stable. A release download uses curl and the system archive and checksum tools. Gateway
provisioning uses the packaged shell installer and the Desktop terminal backend.
USAGE
    exit 0 ;;
  --gateway-only)
    [ "$#" -eq 1 ] || { echo '--gateway-only takes no other arguments' >&2; exit 2; }
    # An explicitly configured, stopped or externally managed instance keeps its owner.
    existing() {
      [ -n "${MUQUN_LOCAL_GATEWAY:-}${MUQUN_GATEWAY_CONFIG_DIR:-}${MUQUN_GATEWAY_STATE_DIR:-}${HERDR_PLUGIN_CONFIG_DIR:-}${HERDR_PLUGIN_STATE_DIR:-}" ] && return 0
      for binary in muqun-gateway herdr-gateway; do
        command -v "$binary" >/dev/null 2>&1 && return 0
      done
      config=${XDG_CONFIG_HOME:-"$HOME/.config"}
      data=${XDG_DATA_HOME:-"$HOME/.local/share"}
      support="$HOME/Library/Application Support"
      for file in "${MUQUN_GATEWAY_INSTALL_DIR:-$HOME/.local/bin}/muqun-gateway" "${MUQUN_GATEWAY_INSTALL_DIR:-$HOME/.local/bin}/herdr-gateway" \
                  "$config/systemd/user/dev.osuki.muqun-gateway.service" \
                  "$HOME/Library/LaunchAgents/dev.osuki.muqun-gateway.plist"; do
        [ ! -e "$file" ] && [ ! -L "$file" ] || return 0
      done
      for base in "$config" "$data" "$support"; do
        for name in muqun-gateway herdr-gateway herdr/plugins/config/herdr.gateway herdr/plugins/herdr.gateway; do
          for item in config.json local-client.json pairing.json gateway.lock; do
            [ ! -e "$base/$name/$item" ] || return 0
          done
        done
      done
      if command -v pgrep >/dev/null && pgrep -u "$(id -u)" -x "muqun-gateway|herdr-gateway" >/dev/null 2>&1; then return 0; fi
      case "$(uname -s)" in
        Linux)
          if command -v systemctl >/dev/null; then
            state=$(systemctl --user show -p LoadState --value dev.osuki.muqun-gateway.service 2>/dev/null) || state=
            case "$state" in ""|not-found) ;; *) return 0 ;; esac
          fi ;;
        Darwin)
          if command -v launchctl >/dev/null && launchctl print "gui/$(id -u)/dev.osuki.muqun-gateway" >/dev/null 2>&1; then return 0; fi ;;
        *) echo 'Unsupported Gateway platform' >&2; exit 1 ;;
      esac
      return 1
    }
    if existing; then
      echo 'Existing Gateway installation or service detected. Keeping its manager and configuration.'
      exit 0
    fi
    [ -f "$0" ] || { echo "Run --gateway-only from the installed Desktop package." >&2; exit 1; }
    helper=$(dirname "$0")/install.sh
    [ -f "$helper" ] || { echo 'Gateway installer missing; use https://muqun.dev/gateway.sh' >&2; exit 1; }
    if ! command -v tmux >/dev/null 2>&1 && ! command -v herdr >/dev/null 2>&1; then
      installers=$(dirname "$helper")
      case "$(uname -s)" in
        Linux)
          desktop="$installers/../../../bin/muqun-desktop"
          [ ! -x "$installers/bin/muqun-desktop" ] || desktop="$installers/bin/muqun-desktop" ;;
        Darwin) desktop="$installers/../../MacOS/muqun-desktop" ;;
      esac
      "$desktop" --ensure-term-daemon
    fi
    channel=stable
    if [ -f "$(dirname "$helper")/VERSION" ]; then
      IFS= read -r version < "$(dirname "$helper")/VERSION"
      case "$version" in *-preview.*) channel=preview ;; esac
    fi
    echo "Installing $channel Gateway using its official installer."
    exec sh "$helper" --channel "$channel" --desktop-terminal ;;
esac
die() { printf 'Muqun installer: %s\n' "$*" >&2; exit 1; }
api=https://muqun.dev/api/releases/desktop
channel=
tag=
while [ "$#" -gt 0 ]; do
  case "$1" in
    --channel) [ "$#" -ge 2 ] || die 'Expected a channel'; channel=$2; shift 2
      case "$channel" in stable|preview) ;; *) die 'Expected stable or preview';; esac ;;
    --tag) [ "$#" -ge 2 ] || die 'Expected a Desktop tag'; tag=$2; shift 2
      [ -n "$tag" ] || die 'Expected a Desktop tag' ;;
    --without-gateway) shift ;; # Older CLI spelling; every installation is Desktop-only.
    *) die "Unknown argument: $1" ;;
  esac
done
system=$(uname -s)
case "$system/$(uname -m)" in
  Linux/x86_64) dest=${PREFIX:-"$HOME/.local"} ;;
  Darwin/arm64) dest="$HOME/Applications"
    [ ! -e '/Applications/Muqun Desktop.app' ] || dest=/Applications
    dest=${MUQUN_DESKTOP_APPLICATIONS_DIR:-"$dest"} ;;
  *) die "Unsupported Desktop platform: $system $(uname -m)" ;;
esac
umask 077
tmp=$(mktemp -d "${TMPDIR:-/tmp}/muqun-desktop.XXXXXXXX")
trap 'rm -rf "$tmp"' EXIT
transfer=
cancel() {
  if [ -n "$transfer" ]; then
    kill "$transfer" 2>/dev/null || :
    wait "$transfer" 2>/dev/null || :
  fi
  exit 1
}
trap cancel HUP INT TERM
progress() {
  if [ "${MUQUN_INSTALL_PROGRESS:-}" = 1 ]; then
    printf 'MUQUN_PROGRESS %s\n' "$*" >&2
  fi
}
download_sample() {
  received=0
  if [ -f "$1" ]; then received=$(wc -c < "$1"); fi
  received=$((received + 0))
  if [ "$received" != "$previous" ]; then
    progress download "$received" "$2"
    previous=$received
  fi
}
fetch() {
  meter=--silent
  if [ -t 2 ] && [ "${4:-}" = package ]; then meter=--progress-bar; fi
  url=$1 output=$2 limit=$3 kind=${4:-}
  set -- -fSL "$meter" --proto '=https' --proto-redir '=https' --retry 2 --connect-timeout 15 \
    --max-time 300 --max-filesize "$limit" -o "$output" "$url"
  if [ "${MUQUN_INSTALL_PROGRESS:-}" = 1 ] && [ "$kind" = package ]; then
    previous=
    download_sample "$output" "$limit"
    curl "$@" &
    transfer=$!
    while kill -0 "$transfer" 2>/dev/null; do
      download_sample "$output" "$limit"
      sleep 0.25
    done
    wait "$transfer" || die "Cannot download $url"
    transfer=
    download_sample "$output" "$limit"
  else
    curl "$@" || die "Cannot download $url"
  fi
  [ "$(wc -c < "$output")" -le "$limit" ] || die "Download exceeds size limit: $url"
}
if [ -z "$tag" ]; then
  channel=${channel:-stable}
  fetch "$api/$channel/version" "$tmp/version" 256
  tag="desktop-v$(cat "$tmp/version")"
fi
# Only a concrete, normalized tag reaches the download URL.
if ! LC_ALL=C awk -v tag="$tag" 'BEGIN {
  n="(0|[1-9][0-9]*)"; exit !(tag ~ ("^desktop-v" n "[.]" n "[.]" n "(-preview[.][1-9][0-9]*)?$"))
}'; then die 'Invalid Desktop version'; fi
case "$tag" in *-preview.*) lane=preview ;; *) lane=stable ;; esac
[ "${channel:-$lane}" = "$lane" ] || die 'Tag and channel disagree'
case "$system" in
  Linux) archive=muqun-desktop-linux-x86_64.tar.gz ;;
  Darwin) archive="muqun-desktop-${tag#desktop-v}-macos-arm64.zip" ;;
esac
base="$api/$tag"
fetch "$base/$archive.metadata" "$tmp/metadata" 512
# The Worker projects these fields from the tagged manifest. Validate its
# fixed grammar before passing sizes to curl or comparing the two digests.
LC_ALL=C awk '
  NR != 1 || NF != 4 || $1 !~ /^[1-9][0-9]*$/ || $1 > 536870912 ||
    $3 !~ /^[1-9][0-9]*$/ || $3 > 1024 ||
    $2 !~ /^[a-f0-9]+$/ || length($2) != 64 ||
    $4 !~ /^[a-f0-9]+$/ || length($4) != 64 {bad=1}
  END {exit !(NR == 1 && !bad)}' "$tmp/metadata" || die 'Invalid Desktop release metadata'
IFS=' ' read -r bytes digest checksum_bytes checksum_digest < "$tmp/metadata"
if command -v sha256sum >/dev/null 2>&1; then
  hash() { sha256sum < "$1"; }
else
  hash() { shasum -a 256 < "$1"; }
fi
verify() {
  size=$(wc -c < "$1")
  measured=$(hash "$1") || die "Checksum tool failed for $1"
  measured=${measured%% *}
  [ "$size" -eq "$2" ] && [ "$measured" = "$3" ] || die "Checksum or size verification failed for $1"
}
fetch "$base/$archive.sha256" "$tmp/checksum" "$checksum_bytes"
verify "$tmp/checksum" "$checksum_bytes" "$checksum_digest"
LC_ALL=C awk -v digest="$digest" -v file="$archive" '
  NR != 1 || NF != 2 || tolower($1) != digest || ($2 != file && $2 != "*" file) {bad=1}
  END {exit !(NR == 1 && !bad)}' "$tmp/checksum" || die "Checksum verification failed for $archive"
printf 'Downloading %s (%s bytes)\n' "$archive" "$bytes"
fetch "$base/$archive" "$tmp/$archive" "$bytes" package
progress verify
verify "$tmp/$archive" "$bytes" "$digest"
printf '%s  %s  SHA-256 verified: %s\n' "$tag" "$archive" "$measured"
progress install
# Extract only the installer executable to a private file. The verified
# executable validates all archive members before staging the package.
if [ "$system" = Linux ]; then
  tar -xzOf "$tmp/$archive" muqun-desktop-linux-x86_64/bin/muqun-desktop-install > "$tmp/install"
else
  unzip -p "$tmp/$archive" 'Muqun Desktop.app/Contents/Helpers/muqun-desktop-install' > "$tmp/install"
fi
chmod 700 "$tmp/install"
"$tmp/install" "$system" "$tmp/$archive" "$dest" "${tag#desktop-v}"
printf 'Installed Muqun Desktop %s under %s.\n' "$tag" "$dest"
